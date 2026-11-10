# Coordinator boundary failures: actual effects before fixes

POLICY_READ coordinator main3ea2164dd98878426936c880f0e8d542ef67d98f;
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root owns the builds, real KVM executions, diagnosis, evidence review, subsequent
mutations and combined gates. These are retained red results, not passing
managed-runtime coverage.

## Pending cancellation did not precede the next send

Rootr597 source0456e99ef475e9e374af0685232c236a3d7b0a4c composes strict
200089e47bc125037a350e0a3954863a86bb6d47, normal tenant fce89fb1 and root wiring.
Normal managed_network and managed_output artifacts compiled0 with carried,
source-matching helpers. The actual [executed driver](boundary0456/driver.executed.py),
artifact hashes, exact commands, environment, kernel/config identities and all
guest stdout/stderr/status are retained.

Both connected and unconnected precommit tests returned101. Their records show:

1. Validated owned ingress, NotAttempted upstream progress.
2. Cancellation requested through the existing worker eventfd.
3. Trusted barrier released after that request.
4. The independent backend receives its third Binary datagram
   5245515545535400ff, from the separate owned relay upstream endpoint.
5. Relay terminal Cancelled at UpstreamReceive/KernelAccepted with ingress1,
   upstream_sent1, no upstream reply or client send.

Thus cancellation was reported eventually, but did not prevent the next send
despite already being pending before its attempted release. This is an actual
backend effect, not missing telemetry or an inference from a send counter.
Both cases retain original positive work time after joins (~29.9s), actual
waiting-child cancellation/reap, joined relay/backend, all links detached and
owned cgroup removed. Effects beyond the observation window and full kernel
charge return remain unverified. Cancellation cannot roll back the observed send.

The original strict expectation remains no upstream attempt/send, baseline-only
backend receipts and complete owned cleanup. No timeout enlargement or accepted
effect was substituted. Correction was delegated only after this live red:
check already-pending cancellation at the existing IO attempt boundary, preserving
native failure and original deadline semantics. A check does not establish an
atomic cancellation/commit boundary against cancellation that arrives afterward.
Fixed/mutation/restored results are still owed.

## Normal member End was followed by premature connection close

The normal-tenant gate also returned101 in r597 before reaching sibling causal
progress or its final metadata assertion. r6010765c7e8 and r60484e63db0 added
diagnostics only, keeping strict behavior. Original records remain separate.
r604 retained the exact member1 stdout canonical sequence:
Open wire0, Data wire28, Data wire58, End wire86; payload tail26 and wire tail102.
The final ACK could not be queued because its client driver had ended; actual
queued transport event was Lost(Some0), not EOF. Lost0 alone did not identify why.

Rootr606 sourcefc7b104b75286418b5d5f534dcf4faca628cd207 added only:
the previously discarded finish error log and a single pre-stop metadata/queued
controlled-event snapshot. The ordinary abort behavior and strict assertions
were unchanged. Actual daemon log:
`managed output diagnostic finish member=1 channel=1: the driver's admitted command slots are full`.
The [pre-stop snapshot](normalfc7b/normal-tenant-exit/records/protocol-failure-observations.json)
shows member1 stdout already aborted, accepted102/consumed86/tailunknown,
while the other three observers had queued Open records. The same canonical
End and Lost0/final-ACK failure were retained. The finish error was printed in
the captured log and again in its diagnostic daemon tail; those are not two
independently counted finish operations.

Source tracing of that actual error: Control::finish commits Ending::Finishing,
then enqueues a pure Unit wake behind Producer::submit's Unit in a bounded
one-slot handoff. Full is converted to Error and Output calls abort. Root approved
a narrow fix for that wake at Control::finish, with queued notifications coalesced
and disconnected-driver failure preserved. Increasing the queue, changing generic
typed-command ownership or altering another unproven submit path is not approved.
Native light full/disconnected cases and root actual before/after/mutation gates
remain required.

Each failed tenant run still completed owner stop and observer joins; r601/r604/
r606 logged exact observer FD7→7. Explicit private run removal and sibling/main
causal checks were not reached. Canonical EOF is not a child wait status or
proof of empty population. The earlier metadata-publication source prediction
has not yet been isolated by a passing transport gate; it remains unverified.

## Provenance, policy and remaining work

All four raw trees have member manifests and privately transferred archive
identities in [ARCHIVES.json](ARCHIVES.json). Sizes/hashes were independently
checked after transfer. No executable or credential was committed. Original
archives and raw records remain on Fedora; local archives in
/Users/adalundhe/g121-retained-archives. Drivers for diagnostic reruns are copied
before execution; r597's driver.executed.py was retained afterward from the
identical executed tool payload, not represented as a pre-existing file.

R01/R04/R06/R07/R13: diagnostic probes change neither the state contract nor
strict terminal/effect expectations; actual typed stages, progress and failures
remain distinct. R02/R03/R05/R09/R10/R12: exact failures precede any correction;
existing native queue/eventfd/Deadline seams, explicit finite caller inputs and
OS-assigned ports are used. No new cache, periodic probe, manager, default,
permission, host setting, installed binary or crypto change. The source-approved
trusted pre-send barrier alone excludes cancellation and uses the original
absolute deadline; the real send path retains ordinary cancellation.

R08/R14: these are functional diagnostic runs, not comparative measurements;
no resource/latency/leak-free or4–5× result. R11: actual amd64 KVM only here,
experimental kernel as identified per record; supported matrix/other hardware
for corrections remains owed, Windows ARM paused. R15/R16: scoped implementers
only source/light checks; root owns campaigns and source compositions, preserves
all red/cleanup records and protected targets. Raw whitespace remains unchanged,
classified in WHITESPACE.json. No safety or rule weakening closes these failures.

Next actual gates: pending-cancel correction in both modes, independent guard
removal/restoration and receive/ordinary regressions; normal finish wake correction,
withheld ACK/disconnect/full handoff/owner stop and sibling progress, then actual
terminal metadata if transport succeeds. Full hard budgets, mandatory routes,
effect broker, native harness adapters/credentials and generations remain open.
