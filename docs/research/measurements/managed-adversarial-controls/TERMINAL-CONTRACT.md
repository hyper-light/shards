# Terminal witnesses and the remaining Aborted case

Inspected current main cbd392 at the exact managed-output seam, not a repository
audit. This records source contracts and fixture choices, not a reproduced bug.

The C caller now waits for actual canonical End, its application ACK and the
corresponding transport End before sending its selected duplicate/forward/final
ACK control. Service::consumed only accepts an exact AwaitingAck record end or
the already acknowledged Ready cursor; Finished is separate. Service::done still
observes Subscription::Closed. The peer may therefore report a typed control
refusal after its data direction ended. This remains an actual-test expectation
until the exact caller runs. Final daemon metadata must preserve ended/complete.

This uses the existing distinction between send and receive directions described
by [RFC9000 section3.4](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.4).
Transport EOF alone does not supply Shards' application ACK, canonical cause,
daemon lifecycle outcome or successful/durable metadata append.

A controlled ordinary actor exit is insufficient to select canonical
Aborted(errno). Linux pipe readers get EOF after the last writer closes
([Linux man-pages pipe(7)](https://man7.org/linux/man-pages/man7/pipe.7.html)).
Current `init::capture::Capture::read` maps read0 to canonical End. Its
nontransient read-error arm preserves a positive errno in canonical Aborted;
preparation/allocation failures instead travel through typed MemberCause and
quarantine/abort-output control. Killing an actor or observing owner abort does
not establish that a canonical Aborted record was received.

Consequently these C End cases make no canonical-Aborted claim. The remaining
case needs a separately bounded controlled capture fault or an actual native
Endpoint/Producer caller, with the latter explicitly limited to its wire/credit
boundary and not presented as daemon metadata or guest-population proof. The
existing native419 fixture was inspected for its public Endpoint/Producer/
Control/Observer seam; its reconnect/initial-cursor cases and source remain
untouched. No descriptor theft, supervisor-handle access, actor impersonation or
new production injection hook is introduced to manufacture that cause.

No timeout/retry/replacement connection, widened capacity or altered expected
reason is approved by this record. Any actual miss is retained before deciding
whether its origin is the fixture, transport, canonical capture or lifecycle.
