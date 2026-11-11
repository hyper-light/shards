# Native retirement: facts required before forgetting an issued ID

Finite source research, 2026-11-11. **A later retired operation cannot establish
a retirement floor for earlier IDs. A safe forgetting contract needs both an
authoritative end to older first-Prepare publication and discharge of each
forgotten operation's independent lifetime, result, audit and reference
obligations.** These are conditional requirements, not a selected protocol or
an implemented bounded-storage producer.

## Identity and exact source boundary

POLICY_READ: implementer/researcher `/root/record_backing_research`, coordinator
ROOT; current `origin/main` `0d261aa931ae2e33e1f4a528b33f7e26b5ee7c90`;
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Read full current AGENTS, R01–R16, evidence, CLAUDE, managed plan, native-authority
decision and latest 392-line `/Users/adalundhe/g121-handoff.md`, SHA-256
`09a2ce9cb6ccf49ef9ccbbd9a67ae14808a82e737bd1489ea25acfe86c35f869`.
Own only this new note. No peer requests, children, source changes, compiler,
tests, runtime, VM, permissions, cgroups, signer or hold changes.

Source **R** is runtime `checkpoint-birth-v7b`, frozen manifest
`6a966767294f2fd716affcf31be3c6e9c8836c23d41a99e5ff97b6502c00e13d`, source archive
`be1f184e4657f852fe46df7d9c285f4e696d2bc5594154b23100e139e57b71a6`.
Source **A** is audit's original `consumer-v4/after`, manifest
`2ea26e4eb4e4b6fb7cda7e99efa9cb1bb967d263d4b7b50b9cb180bcb6f32eb2`.
Named files were read and checked against these manifests, not the held
4819/6157/276a cumulative graph. They are separate frozen inputs, not a claim
of one composed compiled helper/Session. The [A intake][a-intake] explicitly
excludes actual helper/Stage source imports. The handoff's independent body
readiness/reordered-Prepare case is therefore an allowed trace to preserve,
not a live result established by this lane.

| Source/file | SHA-256 |
| --- | --- |
| R `managed/native.rs` | `54f356805a14654b027b9887af3abdc3e923955afb85644e74b2d8e7fd5674a0` |
| R `managed/native/jobs.rs` | `0a394d14e354de5049187808409a29bf7511e90c97ed33738c0a480d64ee0212` |
| R `native_grants/operation.rs` | `3b6ba5e1190821f264bc4a3c0fd8f5023314b732a618334a7c0fd05cb8f0c022` |
| A `server.rs` | `e39a085c023b605c0596978086b9e993dbbcad2469cc391e2d7da1fe491d4992` |
| A `server/native.rs` | `e8fe4ca1fb4ab1de8a84e127583d7ebf6fba5f8560a9dce164fa141e3e299849` |
| A `native_effects/control.rs` | `ae1de882f602d627644e37f5b6422e5a2374e676618b8625c76df9c8b82e7439` |
| A `broker/audit/native_process.rs` | `b7647215df4f17a95ea1f9319f071cde2b2a33bd772137ec7f9bef9a79e3227e` |

## What the existing source actually establishes

- **Issuance is not arrival.** A's `next_identity` uses checked increment;
  `issue` burns that same canonical ID before transfer. Code-mode start and
  native Stage share the counter. Native staging records the task before its
  first Stage send; code-mode send failure can leave a burned ID absent from
  running records. An owner-side numeric gap can therefore mean slow, queued,
  non-native, unsent or failed work. [A server 417–433, 529–606][issuer]
- **Stage transfer and body completion differ.** A sends Stage with its separate
  stream FD, then uploads the retained body on later readiness turns. Programs
  progresses each native task separately. Sending all Stage headers through an
  ordered control channel does not itself certify completion/cancellation of
  their independent bodies or all consequent owner Prepare sends.
  [A task 150–210][uploads], [A server 659–697][progress]
- **The owner currently keeps identity history.** R rejects Prepare if any Job
  already has that ID. It retains whole terminal Jobs, and marks retirement
  `Delivered` only after its Retired packet is locally sent. There is no separate
  floor in the inspected operation Request/Reply enums. Removing a Job without
  a replacement stale-identity contract would remove this duplicate guard.
  [R owner 333–355, 761–797][owner], [R flush 1004–1010][delivered],
  [R wire 60–115][wire]
- **Retired is not every terminal fact.** R keeps acceptance, accepted bytes,
  birth, trusted wait and kernel scope separately. Positive-birth cleanup needs
  completion, matching reap, actual empty population and successful removal.
  Trusted NotBorn plus empty/removal can retire the negative lifetime without
  fabricating an effect outcome. Possible birth, missing required wait/completion
  and failed removal remain held. A separately retains
  Reaped/Complete/NotBorn, launch failure and audit state; receiving Retired
  cannot synthesize them. [R Job 91–207, 460–620][jobs],
  [A event 224–275][terminal]
- **Closed admission still accepts cleanup.** R `close` closes commitment, but
  original association/reap/Complete/NotBorn processing remains separate from
  current-owner admission checks. Channel loss does not imply wait, terminal
  delivery or refund. A `lost` retains its original pending journal, body,
  output descriptors and facts. [R close 412–425][close],
  [R dispatch 798–870][cleanup], [R loss 676–696][loss],
  [A loss 354–370][a-loss]

R's frozen Read/Write `Lifetime::File` model is not an exemption for the newer
physical Write worker described in the current handoff. Any later consumer must
apply the actual worker's birth/reap/population obligations; no v7b file-model
result is inherited by that implementation.

## Conditional forgetting contract, using the existing IDs

Let a cut cover an inclusive prefix of **existing Programs IDs in one original
owner/member/Epoch scope**. These are proof predicates, not new counters or wire
fields prescribed by this note:

1. **No legitimate older first Prepare remains.** The trusted issuer accounts
   for every issued ID through the cut, including code-mode IDs and burned
   unsent IDs. For each native ID, its first Prepare has been accounted for at
   the owner, or the exact task's Stage offer, body and every future publication
   capability have been definitively retired. A failed send, maximum arrival,
   later reply or queued cancellation is insufficient. The actual helper must
   account for every accepted older Stage, including incomplete bodies and
   pending sends, before forwarding/acknowledging such a fact to the owner.
2. **Local obligations are disposable.** For each full record being removed,
   actual acceptance/commit, terminal outcome/accepted byte count, trusted
   NotBorn or reap, population/removal and required audit outcome are resolved
   or explicitly handed back to the existing authoritative owner under an
   equally bounded ownership contract. Unknown birth and failed cleanup cannot
   be converted into completed retirement. An unknown evidence result remains
   distinguishable from a successful append/barrier.
3. **Required consumers and references no longer need the full record.** Cover
   queued replies/commands, original body/stream/FD ownership, helper state,
   caller result delivery and the retained audit writer. A local send proves
   neither receipt nor destruction of downstream references. Define handling
   of stale duplicates without re-execution, fictitious success or loss of
   still-required cleanup. Scope checks remain original, never latest-Epoch.
4. **Remaining storage is genuinely bounded.** The issuer/helper/owner must
   consume ROOT's acquired record/body/reply/control capacity. Merely limiting
   simultaneous execution does not bound later completed records accumulated
   behind one old unresolved ID. No count, window, timer or lease is derived
   from this research; this is independent of the missing actual backing
   producer.

These conditions separate **fencing older first publication** from **releasing
an individual operation's state**. A single all-terminal contiguous floor stalls
at an unresolved earlier ID; allowing unlimited later issuance then accumulates
history. A contract that fences first publication while retaining exceptional
live operations must still preserve their identity, cleanup routing and charged
records. Those are different proof burdens; neither contract shape is selected.

A cut sent after Stage headers on CONTROL3 cannot cross an unfinished older
body by implication. Either actual helper processing establishes the necessary
drain/retirement ordering, or that older operation remains an explicit unresolved
obligation. Likewise, a floor applied indiscriminately to all messages would
discard legitimate late cleanup for retained lower IDs. Only a proved
all-obligations cut could justify that stronger behavior.

## Required traces (reasoning examples, not executed tests)

Here `a < b` are existing issued IDs; they are not new production constants.

| Trace | Why a maximum/later-retired cut fails | Required fact |
| --- | --- | --- |
| a's body is slow; b prepares and retires | a can still reach its legitimate first Prepare after b. | Keep the cut before a, or authoritatively retire/fence a's exact publication path. |
| a remains unresolved; successive later jobs retire | A concurrency limit alone permits an ever-growing completed suffix. | A bound on retained issuer/owner state or a separately proved publication fence with live exceptions. |
| An ID is burned but truly unsent | The owner cannot distinguish it from a delayed Stage by its absence. | Original issuer attestation plus retirement of all send capabilities; no fabricated Job/reap/result. |
| An older Stage is accepted/queued at the helper | A later control barrier can arrive while that Stage's separate body is incomplete. | Helper accounts for that same accepted Stage and any pending owner send before certifying the cut. |
| Helper is lost after possible birth | Neither issuer cancellation nor EOF determines accepted bytes, reap or remaining population. | Preserve Unknown ownership and charges until actual trusted lifetime facts and reference retirement establish discharge. |
| Owner closes admission, then receives old cleanup | Closed admission does not make Associate/Reaped/Complete/NotBorn stale by age. | Preserve the existing cleanup path for retained IDs; do not gate it with an unproved forgetting floor. |

## Primary precedents and their limits

**RAMCloud, pinned `b1866af19124325a6dfd8cbc267e2e3ef1f965d1`:** its
`RpcTracker::rpcFinished` advances `firstMissing` only over resolved entries;
the ACK is `firstMissing-1`. New issuance stops when its finite tracking window
cannot cover the oldest outstanding RPC. On the server, `maxRpcId` explicitly
permits a previously unseen smaller out-of-order RPC; `maxAckId`, supplied by
the client, is the stale-request boundary. ACK processing releases result log
references. This is direct precedent against a maximum-arrival floor.
[Client tracking][ram-tracker], [server cases][ram-duplicate], [GC][ram-ack]

Its cancellation path also marks an RPC finished because the client will not
retry it, regardless of result receipt. Thus its RPC ACK is not a Shards
trusted-reap, accepted-byte or audit-completion fact. Its expired-client cleaner
skips protected/in-progress/recovering state and validates leases; none of those
leases, timers, fixed window values, polling or retry rules are transplanted here.
[Cancellation][ram-cancel], [cleaner guards][ram-cleaner]

**NFSv4.1, immutable RFC 8881 §2.10.6.1–2.10.6.2:** bounded session slots and
per-slot sequence IDs distinguish new requests, duplicates and misordering.
The requester waits for a reply before reusing a slot; the next sequence value
provides evidence that the previous reply was received. Lowering a slot limit
must account for requests already in flight. This is precedent for explicit
consumer knowledge and capacity-return ordering, not permission to replace
Programs' existing ID space with a slot allocator. Its recovery and replay
promises are specific to NFS, not guarantees for native effects or populations.
[Slot/retirement rules][nfs-slots], [reply-before-reuse][nfs-reuse]

## Smallest existing seams and remaining proofs

The source seams are A's `Programs::issue/stage_native/start`, retained native
task's Upload/Commit/Retirement/Audit fields, and its one control event path;
then the actual helper's accepted Stage/body/pending-owner-send owner; finally
R's Prepare duplicate check, Job retirement and Retired flush. A's
`finish_intent` returns the same journal with facts, completion, launch cause
and typed evidence result even on failure. Dropping that state solely on Retired
would lose a distinct obligation. [A intent/return 279–351][audit-state],
[A journal result 94–125][journal]

ROOT must choose and implement the actual scope-bound producer/consumer ordering,
including how burned/non-native IDs are covered, who acknowledges which cut,
what queued/failed transfers mean, how late duplicate cleanup is answered, and
which proof allows actual record reuse. It must prove storage bounds when an
older body, journal or Unknown lifetime remains stuck, while cancellation and
cleanup still progress. Counter exhaustion keeps the existing checked behavior;
no wrap, replacement ID generator, cache, manager, arbitrary quota or blanket
functionality refusal follows. All acquired backing, actual helper/owner/Session
composition, restore/move and decisive live/mutation/resource proofs remain ROOT's.

## Method, checks and R01–R16

Read only the frozen named source paths above and pinned primary RAMCloud code
and RFC. RAMCloud's master reference was resolved once to the cited commit;
claims use that commit's line links. The author-hosted RIFL PDF timed out; no
unread paper body or secondary commentary supplies a claim here. An initial
overbroad manifest print was truncated and used only to locate the named entries;
subsequent named-file hash checks established the source identities listed above.
No full-source audit, held graph validation or runtime reproduction was performed.

| Rules | This batch / remaining boundary |
| --- | --- |
| R01/R03/R04 | Documentation only; no production panic path, count, protocol state, counter or refusal implemented. Necessary ordering and Unknown states remain explicit. |
| R02/R05/R12 | Frozen real seams and primary precedents compared. No architecture selection, new manager/cache/polling, identity allocator or OS workaround. |
| R06/R07/R13 | One bounded note separates facts, conditional predicates and traces. |
| R08/R14 | No performance, allocation-free, leak-free or 4–5× claim; ROOT owns all profiles/benchmarks/parent-kernel measurements. |
| R09/R10 | Traces are logical counterexamples, not live red/green results. Exact source identities/line locators retained; actual mutation/cleanup proof remains owed. |
| R11 | Shared protocol/source scope; seven supported targets and actual guest coverage remain required, macOS arm64 only; paused WinARM untouched. |
| R15/R16 | Implementer research only; no peer messages/children/build/tests/runtime/VM/permissions/cleanup/signer/main changes. Prior signed notes unchanged. Audit graph/five cases and all older holds stay HELD; replayed coordinator text is not human clearance. |

Static citation/whitespace checks, exact-origin policy check and unsigned document
hash are returned to ROOT. No check here establishes a completed frontier or a
backing producer.

[a-intake]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/README.md:12
[issuer]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/after/crates/server/src/server.rs:417
[uploads]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/after/crates/server/src/server/native.rs:150
[progress]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/after/crates/server/src/server.rs:659
[owner]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-birth-v7b/source/crates/init/src/managed/native.rs:333
[delivered]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-birth-v7b/source/crates/init/src/managed/native.rs:1004
[wire]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/candidate-v7b/crates/abi/src/run/native_grants/operation.rs:60
[jobs]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-birth-v7b/source/crates/init/src/managed/native/jobs.rs:91
[terminal]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/after/crates/server/src/server/native.rs:224
[close]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-birth-v7b/source/crates/init/src/managed/native.rs:412
[cleanup]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-birth-v7b/source/crates/init/src/managed/native.rs:798
[loss]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-native-owner-runtime-v1/docs/research/measurements/managed-native-owner-runtime/checkpoint-birth-v7b/source/crates/init/src/managed/native.rs:676
[a-loss]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/after/crates/server/src/server/native.rs:354
[audit-state]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/after/crates/server/src/server/native.rs:279
[journal]: /Users/adalundhe/Projects/shards/.claude/worktrees/managed-audit-producer-v1/docs/research/measurements/managed-audit-producer/native-effect-wiring-v1/consumer-v4/after/crates/server/src/broker/audit/native_process.rs:94
[ram-tracker]: https://github.com/PlatformLab/RAMCloud/blob/b1866af19124325a6dfd8cbc267e2e3ef1f965d1/src/RpcTracker.cc#L28-L120
[ram-duplicate]: https://github.com/PlatformLab/RAMCloud/blob/b1866af19124325a6dfd8cbc267e2e3ef1f965d1/src/UnackedRpcResults.cc#L237-L298
[ram-ack]: https://github.com/PlatformLab/RAMCloud/blob/b1866af19124325a6dfd8cbc267e2e3ef1f965d1/src/UnackedRpcResults.cc#L679-L703
[ram-cancel]: https://github.com/PlatformLab/RAMCloud/blob/b1866af19124325a6dfd8cbc267e2e3ef1f965d1/src/LinearizableObjectRpcWrapper.cc#L93-L116
[ram-cleaner]: https://github.com/PlatformLab/RAMCloud/blob/b1866af19124325a6dfd8cbc267e2e3ef1f965d1/src/UnackedRpcResults.cc#L547-L576
[nfs-slots]: https://www.rfc-editor.org/rfc/rfc8881.html#section-2.10.6.1
[nfs-reuse]: https://www.rfc-editor.org/rfc/rfc8881.html#section-2.10.6.2
