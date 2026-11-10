# Coordinator boundary failures: actual effects before fixes

POLICY_READ coordinator main3ea2164dd98878426936c880f0e8d542ef67d98f;
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root owns the builds, real KVM executions, diagnosis, evidence review, subsequent
mutations and combined gates. These records retain the original reds and subsequent narrow corrections; they do
not establish completion of the managed-runtime plan.

## Narrow corrections and exact closing gates

Pending probe IO correction cd5d7212cc61e9196b5afe7a3e2891ba60a74529 and private
finish notification correction d6f2e4e63cc6e155a041da63a3e42cc61fdd8185 are
composed with the landed allocator instrumentation at candidate
15330d9bd34cde2649e0a36f8e44f76fee520c2c. No diagnostic daemon logging or the
unfinished normal-tenant fixture is part of that candidate.

Rootr626 on probe2b7db81c ran ten strict positive KVM cases0 and one expected
guard-removal case101. [Exact driver and results](pending2b7d/results.json):
corrected precommit connected/unconnected both retained ingress1 and ended
Cancelled at UpstreamSend/NotAttempted, no upstream sends and exactly the two
actual baseline backend receipts. Removing ONLY the new pre-attempt check
produced an actual third backend Binary receipt and KernelAccepted send.
Finally restored bytes/hash and empty git diff are retained, followed by fresh
both-mode precommit, receive-cancel, ordinary two-leg and scripted-responder0.
Every case records joined workers, acquired-link detach and scoped cgroup removal.
The trusted barrier and all strict worker/host predicates were unchanged.

Rootr632 on f31f58f4 confirms the finish wake correction in a real VM:
both member1 channels retained canonical End, successfully enqueued final ACKs
and observed actual transport EOF. There was no finish-Full diagnostic.
The complete strict tenant test still returned101 at its next step: the fixture
had not requested host TCP publication, so read ports listed only four managed
UDP endpoints. The [original record](normalf31f/normal-tenant-exit/records/ports.json)
and canonical EOF proof remain retained. Main progress before release succeeded;
causally later sibling progress, explicit run removal and final terminal metadata
were not reached. A separate publication-only fixture correction is in progress;
no product routing bug or final metadata pass is inferred.

Exact candidate1533 rootr644 normal source-matching host/helpers compiled0:
connected/unconnected strict precommit0, both receive-cancel0, and ordinary
managed_output11passed/0failed/2ignored. [Driver/results](fedora1533/results.json)
retain raw guest identities, native bytes, cleanup and full caller logs.
Rootr634 at the same candidate ran full seven-target scripts/lint0 and actual
macOS-arm64 managed unit tests12/12. Raw commands/logs are in [mac1533](mac1533/).
Toolchain files labelled post are later observations, not a claimed immutable
compiler pin or pre-run environment capture.

Rootr653 independently removed ONLY finish wake coalescing on that Mac:
the two full-handoff native endpoint cases failed with the original Full transport
error; the disconnected-driver control still passed (1pass/2fail, exit101).
These tests construct native bound endpoints/real handoffs without running the
driver loop; they do not constitute another QUIC/VM completion claim.
Exact source restoration/hash plus empty diff preceded a fresh managed12/12 pass.
The [mutation, artifacts and restored logs](finishguard1533/) are retained.

These are functional corrections and source-specific compilation/native/VM
results. Pending-before-check priority does not make cancellation atomic against
a later syscall race. Logical terminal publication, all route classes, hard
budgets, loss/failure campaigns, remaining native hardware and comparative cost
remain open. Windows native execution is an additional pending gate, not claimed
from cross-compilation. The paused Windows ARM VM remains untouched.

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
The closing section above now records the fixed/mutation/restored results.

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
The closing section above records native full/disconnected cases and root
actual transport and mutation results; later logical metadata remains open.

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

Remaining actual gates include notification/worker/readiness failures, concurrent
revocation and expiry; normal-tenant publication, causally later sibling progress
and final terminal metadata; other native hardware and comparable cost. Full hard budgets, mandatory routes,
effect broker, native harness adapters/credentials and generations remain open.
