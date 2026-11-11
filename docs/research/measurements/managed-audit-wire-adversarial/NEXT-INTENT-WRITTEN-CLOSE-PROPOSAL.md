# Next finite proposal: Close after IntentWritten, before commit

SOURCE PREPARATION ONLY. The completed19-frame ReservedClose case is frozen at
e5b3/82e/leaf228b/executable85a6 with original raw9e98. This proposal neither
repeats it nor authorizes a new build, WRITE or actor. ROOT must review the next
exact test-only source diff and dispatch before any such work. No production
defect is alleged; current management gaps remain open.

## Distinct boundary and exact source contract

The accepted cases cover Close after full terminal commit (24 frames at33ea)
and Close before any append (19 frames at82e). The unverified boundary is Close
after a real intent WRITE was accepted but BEFORE CommitIntent or any terminal
WRITE. That carries a nonzero accepted cursor and uncommitted bytes; it must
not be relabelled a completed/synced effect or resurrected by later controls.

Read-only pinnedba78 source: `Member::close` sets Available/Held→Closed without
discarding the lease; `Member::reply` projects Stage::Closed while preserving
accepted/call/maxima/offsets. `Run::control` permits same-call Close/Inspect, but
commits require Life::Held and otherwise return Cause::Closed. `Run::reserve`
also refuses a closed member before allocating. These are source contracts, not
a live reproduction or a claim about concurrent in-flight completion.

Use the SAME actual Server/ShareStream/frame functions/codecs/held-file snapshots
and owned cleanup. Reuse the original6b/5da Send corpus and derive the same ONE
aggregate B from actual118B headers,40B slot overhead and actual codec maxima.
Audit call0, semantic broker request1 and FUSE unique IDs stay separate. Derive
accepted intent length from the original file; do not choose139 as runtime policy
or rewrite/re-emit any encoder bytes. No production API/writer/manager/cache/
queue/parser/transport change, fake I/O or second observer.

## Proposed exact21 ordinary requests

| Unique | Request and required observation |
| --- | --- |
| 1–8 | Unchanged shared INIT, LOOKUP/OPEN1+3, Identify1+3, Reserve1. Actual Reserved/None, accepted0, remaining0; held extentsH+S andH. |
| 9 | ONE actual FUSE WRITE of the original intent at returned intent_offset. Exact acceptedIlen; held bytes change only in that original intent range. No terminal WRITE. |
| 10 | Inspect call0: IntentWritten/None, acceptedIlen, same lease/cursors/remaining0. Capture the SAME held whole-file snapshot as the reference. |
| 11–13 | Close, intentional duplicate Close, Inspect: Closed/None with acceptedIlen retained; every byte equals that held IntentWritten reference. |
| 14–16 | CommitIntent, CommitTerminal and same Reserve AFTER Close: actual Closed/Closed, acceptedIlen and every lease field/remaining0 unchanged. No commit-prefix or other byte change, new extent or refund. |
| 17 | Inspect: Closed/None, exact same retained cursor and files. |
| 18–19 | Member3 Reserve→Empty/Exhausted, then Identify→own Empty/None, remaining0/accepted0/no lease; unchanged neighbor header and closed member bytes. |
| 20–21 | RELEASE1+3: actual outer success; retained aggregate extent and reference bytes unchanged until owner deletion. |

Each exchange records full request/reply and actual typed result BEFORE final
assertion. Selected duplicates are bounded protocol inputs, not recovery retries.
The entire post-Close file comparison must include both length prefixes, unused
tails, neighboring header and the actual accepted intent bytes. No CommitIntent
is sent before Close, no terminal payload/effect is executed, and post-Close
refusals must not reset acceptedIlen to0. Do not infer physical zeroing/sync from
an unchanged logical byte prefix; the snapshot itself is the authority here.

## Cleanup, alternatives and future admission gate

Use exactly the same client IPC end/drop BEFORE answer join, actual exposed-FD
EBADF, held dev/inode/nlink and witness close checks, Server/root-FD drop and
TempDir deletion/root absence. Preserve first failure and mark later facts
unreached/Unknown; parent deadline kill cannot become a cleanup pass. All source,
binary, original corpus, raw and failed receipts from both existing cases stay
frozen. Keep the original24 and19 assertions/helpers semantically unchanged.

Established alternatives remain direct internal ticket tests (miss the actual
wire path), privileged FUSE mounts (unneeded host surface), or the existing real
frame path. Choose the latter only for this ordinary serial state boundary.
Pinned FUSE/ABI/API sources and broader alternatives are in FRAME-AND-CLEANUP,
the original proposal and ba78/8ac/6b. No broader architecture decision follows.

If ROOT approves implementation and later actors: a NEW exclusive normally
admitted writable ARM64 Mac source/target/cargo-home/TMPDIR/records,2CPU/2GiB/
4GiB600/j2/build180/work30+cleanup15/hard45, exact binary/list1/hash/dispatch
BEFORE ONE first native attempt; no runtime retry or time extension. Existing
30GiB floor and lock remain binding. This is not an extension of the expired/
completed19-frame scope and cannot reuse its target. Refresh FULL current policy
and handoff at that new authorization; current readback9494 was b6/df2, and the
later0a6 broadcast does not grant a new actor or passing main gate.

No VM, mount, namespace/root/security change, held helper/allocator/cotenant
campaign, in-flight/stall/fault/ENOSPC/crash/mutation/profile/benchmark or authority
claim. Byte/cursor checks cannot prove durability/physical quota/fairness/tool
effects/hard parent-kernel rights/population return. ROOT owns integration,
producer authority, seven targets, those live campaigns and costs/main merges.
No child/goal/autodispatch. R01–R16 remain binding; all new runtime/compiler/
format gates here are UNRUN, and this document is the next reviewable queue item.
