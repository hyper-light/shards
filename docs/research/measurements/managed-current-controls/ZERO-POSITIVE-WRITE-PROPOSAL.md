# Actual zero and positive write backpressure: proposal only

No Rust source/shim/zero-write branch has been edited and no new live case has
run. Current main/policy read in full at a920c85b5, rules SHA
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3. Completed
2c57/50c/1e2560 source, binaries and raw proof remain frozen. ROOT reviews this
proposal before any instrumentation/case; a new approved scope starts current
main, not a rebase of that campaign.

## Exact authority and primary contracts

At a920, remote/src/net.rs Io::write (line389) calls the real QUIC send stream;
WriteError::Blocked maps to Ok(0). Handler::writable (line204) is the existing
driver's readiness callback. managed.rs Producer::submit (line427) returns the
owned Vec as typed Rejected::Full while its slot cannot accept another record;
capacity/state/progress are authoritative. These are distinct from command
enqueue, network/application ACK, or absence of a trace receipt.

[RFC9000 stream flow control](https://www.rfc-editor.org/rfc/rfc9000.html#section-4.1)
and [STREAM/FIN semantics](https://www.rfc-editor.org/rfc/rfc9000.html#section-19.8)
separate stream bytes, final size and receive progress. Zero submitted bytes plus
FIN would exercise empty-pending finish, not an Io::write return0. That separate
FIN-only idea is deferred; it cannot satisfy this backpressure witness.

## Smallest proposed test-only additions

Use the SAME shared Client/Observer, pending Vec, flush and writable callback.
No extra driver/parser/queue/manager, fake Io return, pause, sleep or retry loop.
Only when the existing test receipt sink is armed, record:

- Each real write attempt's offered pending length and exact backlog bytes,
  then its actual returned count/error and exact backlog bytes after it. The
  controlled packet is bounded by existing CONTROL_BYTES; this is not a cache.
- An explicit actual zero-return event for NONEMPTY pending; never infer it from
  a missing positive receipt. The zero branch still returns immediately.
- The existing writable callback's actual connection/stream match and pending
  suffix before it invokes the unchanged flush. No synthetic readiness event.

Keep every existing positive-write/drain/accounting assertion unchanged. Add
separate trace checks requiring n0 leaves the exact offered suffix intact, a
matching real writable callback precedes resumed acceptance, positive counts
consume only a contiguous prefix, and final accepted bytes equal the original
packet with pending0. Trace data is evidence, never protocol/state authority.

## Proposed finite callers and forbidden effects

1. One fresh native owner-output Endpoint, stdout/supplied errno5. Reuse the
   existing checked Open/Data/Aborted records and data-derived allowance. While
   actual Open awaits its application ACK, require Control Pending, capacity0
   and unchanged progress; submit one valid in-budget owned candidate ONCE.
   Require typed Full returning the same allocation/capacity/exact bytes. Keep
   that returned candidate separate; do not retry it. Then continue the original
   normal records after actual ACK/readiness, under the original deadline.
2. After actual Aborted/ACK/transport End, use the existing real58-byte Subscribe.
   Its native allowance is derived from actual record capacity plus DATA_HEAD
   (previous actual positive writes were55+3). That is a candidate for a real
   blocked call, not evidence one occurred. Require the explicit actual n0,
   unchanged nonempty suffix, actual matching writable callback and resumed
   positive acceptance. If n0 is not observed, retain the result as unverified/
   failing this coverage goal; do not tune capacity, inflate traffic, fake an
   event or silently retry. Require Protocol and unchanged supplied cause/
   Finished cursors, exact postterminal owned Closed return, observer stop/join,
   then owner abort/finish Closed/endpoint join and FD baseline return.
3. One fresh VM mirror using the existing post-End Subscribe stdout case and
   derived existing allowance (previous actual43+15). Require the same real
   zero/readiness/positive trace plus sticky ended/complete, exact new neighbor
   fd1/fd2/main progress, owner stop/inspectfalse/remove/unlink/joins/FD return.

Forbidden effects are actual drained/lost/duplicated pending bytes when n0 was
returned, accepted/consumed credit advancing on typed Full, loss/substitution of
the rejected owned Vec, a false backend control success, altered canonical cause/
cursor, finish-after-abort resurrection, missing neighbor/main progress, or an
owned endpoint/observer that cannot be retired. Exact byte backlog receipts
and typed returned bytes/state establish those effects; missing receipts do not.

## Bounds, alternatives and limits

Same private Linux2CPU/4GiB/24GiB, j4 full caller build<=3600s, each case240s plus
15s cleanup, fresh records/owner/endpoint, actual source/fixture/helper/guest
hashes. Callback logging is nonblocking and performs no Life reentry, metadata
I/O or wait; the existing credit callback stays wake-only. New source and exact
instrumentation hash readback precede tests. No production source or protocol
changes. ROOT owns combined/matrix/mutation/audits/profiles/benchmarks/main merge.

Alternatives rejected: existing positive-only receipts cannot prove n0; FIN-only
never calls write on a backlog; a copied/mock driver bypasses authority; a new
pause/coordinator or tuned window would change the path being tested. No
zero-write defect, complete draining/race/resource/durability guarantee or
performance result is claimed. No helper-control/worker-result allocator or
cotenant-theft scenario is included. Existing positive-write/frozen assertions
and all raw originals remain unchanged.
