# Valid Subscribe after an observed canonical terminal

This next owned C case is `terminal-end-resubscribe`, selected once per new
owned run/endpoint on stdout or stderr. It follows the frozen eed0 checkpoint;
it does not change or rebase that campaign. Parent patches remain exactly the
approved original raw-control and registration patches. No new seam is needed.

After actual canonical End, its ACK and actual transport End, the existing
authenticated observer sends one `managed::subscribe(observer.scope, wire)`.
The identity and cursor are the actual owned scope and observed terminal wire
end. The real encoder returns exactly CONTROL_BYTES, the current bounded
control buffer. This case tests a valid control kind in the wrong protocol
state at that bound, rather than duplicating an initial cursor/scope/stale case.

Inspected source: remote::managed::Service::controls accepts Subscribe only in
Connecting, and Consumed only in Subscribed on the matching connection/stream.
A second Subscribe in Subscribed returns typed Protocol even when the remote
slot is Finished. Slot/canonical terminal/connection/lifecycle remain distinct.
This is an expected contract, not a claimed live result or production defect.

The case uses existing RawControl pending/partial-write handling and actual
write/read/finish receipts. Whole positive accepted input and zero remaining
pending are required after actual refusal and local join. Enqueue or a local
write is not backend consumption. No FIN, reconnection, retry, sleep, capacity
increase or transport implementation is added. Actual partial writes may or
may not occur; a maximum-size input does not prove they occurred.

Alternatives are already established in RAW-CONTROL-PROPOSAL.md and
TERMINAL-CONTRACT.md: typed Ack cannot express a second Subscribe; a new
connection would enter the separately open replacement-admission path; copying
the driver/authentication or injecting production state would skip this seam.
The existing RFC9000 bidirectional stream contract explains why transport send
End does not itself end incoming controls. No broader architectural decision
or new production constant is made.

All original C assertions remain: exact Protocol, no future target record,
sticky member1 ended/complete102/102 stdout and99/99 stderr after owner stop,
actual fresh neighbor fd1/fd2 and main bytes, stop/inspectfalse/remove/unlink,
observer joins and FD return. Full population/kernel costs remain unknown.

Use the same explicit positive phase/work/cleanup/outer bounds, new private
records and admitted2CPU/4GiB/16GiB Fedora scope as LIVE-EED0.md. Full normal
caller compile, two actual selected KVM cases, changed selection pure checks and
scoped Clippy are pending at this source checkpoint. No earlier binary passes
this source. The LIVE-EED0 R01–R16 account applies with those new gates pending;
only the owned leaf/contract/evidence change. ROOT owns shared-file adaptation,
combined matrix, audits/profiles/benchmarks and main merge. Rejected helper
malformed-control and worker-result allocation campaigns remain held.
