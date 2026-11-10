# Subscribe after a supplied canonical Aborted

Actual follow-up: [1e0 raw record](LIVE-1E0.md) passed8 native cases,7 pure checks
and scoped full-caller Clippy0. The source-stage pending gates below preserve
their original chronology.

This C continuation uses the same actual public Endpoint/Producer/Control and
authenticated Observer as frozen eed0. It adds a second explicitly selected
native test, `canonical_aborted_resubscribe_preserves_errno_and_terminal_credit`.
The original duplicate-ACK test remains and runs through the same owner and
cleanup method. Selection is a private enum, not implicit boolean state.

After receiving and ACKing exact Open/Data/owner-supplied Aborted(errno), both
tests require actual transport End and remote Finished at the exact cursor.
The new selection sends one real `managed::subscribe(scope, cursor)` through
the approved test-only RawControl/pending/partial-write seam. It must yield
typed Protocol while the supplied canonical errno/offset and Finished cursor
remain unchanged. No replacement connection, helper control or allocation
fault injection is involved. The remote Service/primary stream contracts and
established alternatives are the same as POST-END-SUBSCRIBE.md.

The existing postterminal owned payload must return with typed Closed and
exact bytes. The observer is then stopped/joined, queued events retained and
future records forbidden; the owner must still be Finished before owner abort.
Raw-control receipts are drained only after this local join and checked by the
existing shared receipt accountant. Owner abort must still retire the endpoint,
preserve cursors and reject finish-after-abort with Closed; both joins/FD return
remain required. There is no canonical cause resurrection or daemon metadata
claim. The supplied errno is never represented as a kernel-observed error.

Original positive native phase/cleanup/outer inputs and one fresh scope/endpoint
per selection/channel/errno apply. Full caller compile, both selections across
stdout/stderr and errno5/i32::MAX, pure checks and scoped Clippy are pending at
this source checkpoint. Earlier sources/binaries/records remain frozen. Only
the owned native child and new evidence change. LIVE-9E7/LIVE-EED0 account for
R01–R16; this new actual native gate is pending, other hosts/matrix/mutations/
daemon-Aborted metadata/population/kernel costs remain ROOT-owed. No broader
architecture/production change, new manager/cache/thread/poll or local build.
