# Next finite native edge: Close while Reserved, before any append

Historical preparation contract retained below. ROOT subsequently approved this
independent case; source82e/leaf228bb9e6 passed its first and only19-frame native
runtime attempt. [Current results](reserved-close-native-v1/RESULTS.md) preserve
the new source/package/raw and preflight restart failure. The original24-frame
case/source/raw stays separately frozen, with no inherited new runtime gate.

Preparation only. The first full24-frame case is frozen and passed once; it is
not rerun or weakened. This proposed distinct state edge has NOT been implemented,
built or executed and needs ROOT's source/actor authorization. No producer defect
is alleged. The lane remains assigned; this proposal is the next finite item.

The exact UNAPPLIED [caller diff](reserved-close-preparation-v1/native-caller.patch)
and [source/check readback](reserved-close-preparation-v1/README.md) are now present
for that review. The actual executed native leaf remains unchanged. Static format
was held by the shared lock; no compile/actor gate is claimed for this proposal.

## Concrete contract and shared source

Use exactly the same frozen ba78 constructor/writer, original6b corpus and actual
`fs::write_frame/read_frame/answer` with one `Server::audited`, one stream pair,
one owned answer thread and the existing Wire/Trace/Corpus/snapshot/cleanup helpers.
No copied codec, new visibility/API/manager/queue/backend or active writer change.
Keep every first-case assertion and source record unchanged.

The source contract is `Member::close` plus `Run::control/reserve`: an Available
held file closes even while its lease is Reserved; Close retains the lease and
extents. Identify/Inspect continue reporting the terminal Closed state; a later
Reserve or commit returns typed Closed instead of reopening the member. The
existing source unit covers in-flight Close through a prepared ticket; that is
a different component boundary, not evidence of this proposed full-wire path.
This finite serial case makes no in-flight-I/O/concurrency claim.

The selected original Send maxima again derive ONE aggregate B from the same
actual two headers and one reserved slot. Do not introduce a per-member B, choose
new maxima, write fake semantic records, or infer physical quota from lengths.
Here there are deliberately NO WRITE requests: Close must leave even the whole
reserved slot unchanged rather than presenting an uncommitted effect as complete.

## Exact19 ordinary requests and observations

| Unique | Request | Required actual observation |
| --- | --- | --- |
| 1 | INIT | Same complete fixed body and checked outer reply |
| 2–5 | LOOKUP/OPEN member1, then member3 | Exactly the two gapped stable files and direct handles |
| 6–7 | Identify member1 and3 | Own Empty/None scope, initial shared remainingS |
| 8 | Reserve member1 call0 with original Imax/Tmax | Reserved/None, accepted0, exact offsets, remaining0; held extentsH+S andH |
| 9 | Close member1 BEFORE any append | Closed/None, accepted0, same lease/cursors/maxima, remaining0; both whole files equal post-Reserve snapshot |
| 10 | Intentional duplicate Close | Exact same reply and files; no second debit/refund |
| 11 | Inspect same call | Closed/None, same lease fields/remaining0 and both files |
| 12 | Same Reserve after Closed | Closed/Closed, accepted0 and same remaining/cursors/whole files; no resurrection |
| 13 | CommitIntent after Closed | Closed/Closed and unchanged whole files; no intent prefix/payload publication |
| 14 | CommitTerminal after Closed | Closed/Closed and unchanged whole files; no terminal prefix/payload publication |
| 15 | Inspect member1 | Closed/None, exact retained lease and files |
| 16 | Reserve member3 with its own original maxima | Empty/Exhausted, no lease, accepted0, remaining0, unchanged neighboring header |
| 17 | Identify member3 | Own Empty/None and unchanged remaining0 |
| 18–19 | RELEASE member1 and3 | Actual outer success, retained bytes/lengths unchanged until owner deletion |

Each new unique ID is a selected finite input, not recovery retry. The same
existing Rust codec decodes every typed control response; outer errno0 alone
does not pass a typed check. All held snapshot bytes, request/reply frames and
actual typed results are recorded BEFORE any final assertion. A failed response
ends the sequence, marks later requests unreached and proceeds through original
owned cleanup; no new connection/reopen/reset or resubmission.

## Cleanup, forbidden effects and intended bounds

Reuse the reviewed existing cleanup exactly: client IPC end/drop before answer
join, immediate exposed stream EBADF, same held file identity/link checks and
witness EBADF, Server drop/root-FD EBADF, TempDir owner deletion and actual root
absence. Trace errors remain failures; external deadline/join kill leaves cleanup
Unknown. Native TempDir own-process limit preparation must again be disclosed
with actual before/after in a new empty private TMPDIR, never a headroom increase.

Intended later scope, only after ROOT approval: fresh normally admitted writable
Mac ARM64 exclusive source/target/cargo-home/TMPDIR/records,2CPU/2GiB/4GiB600/j2,
build180, one absolute work30+cleanup15/hard45 parent, exactly1 selected native
case, no runtime retry/time increase. Freeze/list/hash/dispatch BEFORE actors.
No new full local worktree/heavy local build; keep Mac30GiB floor and existing
lock. Never reuse current source/target as a hidden second attempt.

No VM/mount/namespace/root/security/trust change, helper controls, allocator hooks,
owner/neighbor theft, held campaign, stall/fault/ENOSPC/crash/mutation/profile or
benchmark. No fairness, physical quota/durability, live tool effect, hard parent/
kernel entitlement or population claim. ROOT retains those independent gates and
integration/7/mainmerge authority. No child/goal/auto dispatch. All original24-frame
raw, source packages, frozen binaries, old FIN and held artifacts stay preserved.

## Decision and authority limit

The established alternatives remain the reviewed ones: direct Server/ticket
component calls (already separately tested, miss the wire seam), a host FUSE mount
(adds kernel/admin surface), or the existing actual ShareStream frame path. Choose
the third for this ordinary native seam; no broader transport choice is made.
Pinned FUSE/header/IOCTL sources are in FRAME-AND-CLEANUP and the original proposal;
the authoritative constructor/codecs are the same immutable ba78/8ac/6b objects.
No new production literal or implementation is proposed. This document is the
concrete source/readback proposal; it does not authorize actors by itself.
