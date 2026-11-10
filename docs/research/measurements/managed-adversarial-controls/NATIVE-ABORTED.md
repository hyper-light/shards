# Next owned source batch: canonical-Aborted wire boundary

This batch is separate from the frozen57a8 campaign. It uses the actual public
Endpoint/Producer/Control and existing authenticated Observer/Client, with one
fresh native owner/scope/endpoint. Three checked ABI records (Open, controlled
Data, owner-supplied positive Aborted(errno)) derive the retained allowance from
their actual Vec capacities. There is no transport copy, replacement connection,
capacity increase, periodic probe or production change.

The fixture ACKs actual received records, requires actual transport End and exact
remote Finished cursors, then sends a duplicate ACK and requires typed Protocol.
The canonical Aborted errno and payload offset remain distinct from that remote
Finished wire cursor. An attempted postterminal owned payload must be returned
with typed Closed. Owner abort must still retire the actual driver and reject
finish-after-abort, with observer/endpoint joins and exact FD observations.

The callback sends only a coalesced nonblocking wake token; the existing Control
state is authoritative. Ready waits recheck that predicate after notifications
under the original deadline. No callback reads Life, waits or performs metadata
I/O. This is a native wire/credit boundary, not daemon metadata or a real guest
read-error reproduction. The supplied errno is never labelled a kernel-observed
cause. Actual daemon canonical-Aborted metadata remains unverified.

The ignored test is
`tenant_exit::adversarial_controls::native_terminal::canonical_aborted_duplicate_ack_preserves_errno_and_terminal_credit`.
Its explicit `SHARDS_ADVERSARIAL_NATIVE_` inputs are PHASE_MS, CLEANUP_MS,
OUTER_MS, CHANNEL, positive ERRNO, nonempty fresh RUN, positive GENERATION,
SOURCE_REVISION and a new absolute RECORDS directory. The direct caller enforces
the outer bound. All phase/cleanup deadlines start at the test's first Instant.

The same batch retains successful main stdout/stderr bytes for the VM cases
through the existing exact CLI command seam. It preserves the old stdout,
stderr-prefix and optional timing diagnostic assertions. The earlier57a8 raw
main-byte gap is not retroactively filled.

R01/R03–R07/R12–R13: test-only source, no production literals/interfaces/OS adapter,
explicit canonical/remote states, existing driver and small record sink shared by
the two actual callers. R02 alternatives and primary stream/pipe contracts are in
TERMINAL-CONTRACT.md and the inspected native419/current Endpoint source. R08/R14:
no performance/allocation/kernel-cost claim. R09/R10/R15: no production bug/fix
claim; full caller compile, actual native cases, VM retention rerun and scoped
Clippy remain pending at this source checkpoint. R11: Linux caller only; other
platform/matrix gates ROOT-owed, WinARM untouched. R16: reuse the owned admitted
remote source/target, no local compile or protected ROOT cache.
