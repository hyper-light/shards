# Pre-upstream-send cancellation coverage

Implementer batch starts at signed receive-cancellation checkpoint
`7d01dcaf1ed7b46fe12eea6fcd33b2e50658f520`, branch
`p/managed-relay-precommit`, reusing the same `managed-relay-cancel` worktree.
POLICY_READ renewed full canonical AGENTS, R01–R16, evidence, CLAUDE, managed plan
and updated handoff: main/origin/main24f, policy file revisionf5cec365, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root approved the concrete three-signal/Control-enum proposal before source edits.
Root owns all audits, live socket/namespace/cgroup/BPF/VM execution, adversarial
campaigns, profiles, benchmarks, combined gates and main merges.

This is a **new test expectation/coverage**, not a demonstrated bug or runtime
pass. Root must reproduce any actual effect before a production/Deadline/worker
fix. Receive-cancellation source7d01 remains preserved on its pushed branch.
No production grant/policy/kernel, actor authority, source-port preservation or
new installed artifact is introduced.

## Exact boundary and state

Explicit command, no input default:

```
sock-addr-precommit4 {connected|unconnected} WORK_MS BUFFER_BYTES
```

Reservation, private namespace/cgroup, selected FixedOriginalPeer hooks, fresh
exec children and native sockets are the same existing fixture. Ordinary relay,
scripted responder and receive-cancellation commands remain available. The old
receive test retains its independent backend receipt, exact byte/tuple/count,
terminal/deadline and cleanup assertions; host case selection shares these strict
assertions without weakening the old predicate.

Forward Control is Ordinary / ReceiveEntry / BeforeUpstreamSend, and parent
Cancellation is Receive / BeforeSend. No independent optional flags encode legal
control combinations. In the new variant the worker first consumes, records and
validates actual Binary ingress and pins its actual client tuple. It enters
BeforeUpstreamSend, records IngressValidated with NotAttempted upstream progress
and signals one readiness eventfd. The trusted parent observes this event,
requests cancellation through the existing checked DatagramWorker cancel write,
then releases a second one-shot eventfd, including the error cleanup path.

**Only the trusted fixture barrier** uses `cancel=None`, with the same original
absolute deadline instant. This prevents the barrier from consuming the pending
cancellation intended for the actual next send path. No new deadline or cleanup
grace is created. The real upstream send and cancellation behavior remain
unchanged: [unchanged-controls.json](unchanged-controls.json) verifies exact
byte-identical Deadline.io, worker wake/join, cancellation observation and actual
upstream send bodies against7d01. An immediate operation may still run before
cancellation is observed; that source observation is not a live reproduction,
and this change does not repair it or alter the expected result to accept it.

The third eventfd reuses the existing HeldReply backend's independent actual
forwarded receipt signal. Its baseline replies remain Binary then Empty. If a
request is nevertheless forwarded after the cancellation request, the backend
records and validates its actual third receipt before signalling, then issues no
reply. The parent ends and reaps its waiting owned child, joins the relay, and
**only if completion records a full upstream send** waits for this independent
backend receipt under the original deadline before ending/joining the backend.
A red therefore retains the actual backend request bytes rather than merely a
missing alert. A send count alone still does not establish delivery, and missing
receipt/report/error cannot become proof of non-delivery. This special join
order applies only to BeforeSend; receive cancellation keeps its prior order.
Every selected link detach and scoped cgroup removal is still attempted/reported.

The strict new expectation is Binary / Cancelled / BeforeUpstreamSend-or-
UpstreamSend / NotAttempted, ingress_received1 and all upstream/reply/client-send
counts0, actual baseline-only backend receipts and validations, complete owned
cleanup and positive original deadline remaining after both joins. The retained
NotAttempted/ingress facts and joined backend baseline counts are required along
with exact actual baseline bytes; absence of an alert alone cannot pass. A
KernelAccepted send or any observed attempt fails. No rollback of retained
ingress or any committed effect is claimed. The existing no-response client is
explicitly killed/reaped through the prior OwnedClient path, not left detached.

## Sources, alternatives, literals and waits

[Receive-cancellation evidence](../managed-relay-cancel/README.md) retains the
pinned Linux v6.18 [eventfd](https://github.com/torvalds/linux/blob/v6.18/fs/eventfd.c)
and [poll](https://github.com/torvalds/linux/blob/v6.18/fs/select.c) sources and
retrieved hashes, plus eventfd(2). The
[original relay decision](../managed-network-sock-addr/buffered-relay/README.md)
retains pinned Envoy/Cilium alternatives and Linux send/UDP contracts. The
[forwarding record](../managed-coordinator-forward/README.md) retains actual
baseline and independent backend/byte/hook mutation witnesses. Their earlier
results do not prove this boundary.

The existing eventfd/Deadline/native-decoder/DatagramWorker seam supplies all new
waits; no repeated state probe, sleep, cache, second decoder/worker manager or
unowned thread is introduced. A pipe uses additional descriptors; a channel or
condition variable needs a separate composable deadline/cancel path. A timing
sleep does not establish the requested boundary. Existing unexpected backend
datagram handling records actual bytes and fails with typed UnexpectedDatagram.

The two-direction gate and independent backend receipt are three distinct
one-shot events, not runtime capacity choices. One held Binary ingress and the
existing Binary+Empty baseline derive the counts; uint64 notification words,
truncation flags and signals are ABI facts. Ports remain OS-assigned, envelope
and deadline caller supplied. Existing fallible allocation/capacity/overflow
behavior remains. PayloadCapacityOnly still excludes allocator/parent/kernel/
socket/console costs. No performance or total hard-resource claim is made.

## Actual light results and source freshness

Fedora amd64 runner8fadc1ec213332e16cb0d915a3fb913f, rustc1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`; admitted **1CPU/1GiB/2GiB** via Kiln.
All own jobs complete, NEW private source/target directories only:

| Implementer job / private proof | Actual result |
| --- | --- |
| r5 / managed-relay-precommit-proof-v1 |34 GNU+34 musl pure tests0, both scoped production panic/unsafe lint subsets0. |
| r6 / managed-relay-precommit-host-v1 |Sibling actual parente1637959 + copied new module compile0 with inert type-matching common stubs; new precommit, existing cancel and three prior exact pure filters each1/pass1. No VM helper runs. |
| r9 / managed-relay-precommit-proof-v2 |After correcting one shared comment's applicability, fresh final source repeats34 GNU+34 musl pure tests0 and both scoped lints0. |

These are scoped pure/light results, not full normal host/guest/helper builds,
workspace/seven-target or hardware validation. New actual socket/ns/BPF/guest
execution, profiles and mutations are **unrun here**. Local owned rustfmt,
development-policy and source/docs diff checks pass.

Final source hashes match actual v2 guest and v1 host copied source manifests:
[source-identities.json](source-identities.json). Exact command JSON/logs,
scaffolding, Cargo locks and three initial binary identities are under [light/](light/);
final two guest binary identities/logs are under [light-v2/](light-v2/). Both text
archives remain remote, and all54 regular members/52 content hashes were checked
locally; no executable bytes transferred. v1 archive7481B SHA256
`3863209e84a449831a49ea50dbc3a0bbe6aa80f046834573832efcbace164c2a`;
v2 archive4368B SHA256
`8d83c162663105e9bf2605bf9ff3305ab44b8f4cdcb5f46bd5ea2b65e18b4a2a`.
Sources, targets, binaries and both archives remain preserved in their private
runner directories. No root cache or source directory was written. Mac free at
start32838996KiB (>30GiB), no new worktree/full copy/local build; paused Windows
ARM remains untouched. Nine staged raw Cargo terminal-blank-line diff warnings remain hash-identical;
no source/global whitespace rule was relaxed.

Pure/light replay, NEW private directory arguments and admitted envelope:

```sh
python3 docs/research/measurements/managed-relay-precommit/light.py guest "$HOME/managed-relay-precommit-proof-NEW"
python3 docs/research/measurements/managed-relay-precommit/light.py host "$HOME/managed-relay-precommit-host-NEW"
```

## Coordinator invocation and owed gates

Root composes the same sibling shared assertion helper/module declaration as7d01;
this implementer does not edit host managed_network.rs. Use the forwarding record's
explicit kernel/config/source/helper/boot/work/memory/vCPU environment, fresh
records, and source-matching normal artifacts:

```sh
SHARDS_SOCK_ADDR_MODE=connected \
SHARDS_SOCK_ADDR_BUFFER_BYTES=18 \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  cancel::direct_guest_udp4_precommit_cancel_requires_no_backend_effect \
  -- --ignored --exact --nocapture --test-threads=1
```

Repeat unconnected.18 derives from the actual9-byte request+9-byte reply envelope;
it is not a default. Retain actual source/binary/kernel/config hashes, command,
stdout/stderr/status before strict assertions. If a red has a real third Backend
Binary receipt and KernelAccepted progress, retain its exact bytes/tuple/stage
and cleanup before proposing any production remedy. Do not weaken assertions.

Root owes live expected-no-effect versus actual outcome, receipt-on-red,
original-deadline expiry, release/notification/record/OS failure, cancellation
mutation/restoration, complete child/worker/link/cgroup cleanup, receive-cancel
regression and ordinary relay/responder/guard controls, full combined seven-target
and real supported hardware gates. Receive-cancellation findings take priority.
No actual defect/fix/completion, leak freedom, hard budget, neighbor fairness,
allocation/page-fault or comparative performance claim follows from this fixture.

## R01–R16 delta record

| Rule | Evidence / remaining root work |
| --- | --- |
| R01 |Fallible descriptor/clone/event/I/O, checked counters/slices/capacity; existing denied production panic lints retained; final GNU/musl scoped lints0. OOM/OS/error paths unverified. |
| R02 |Concrete root-approved finite proposal; exact existing Linux eventfd/poll/send and linked pinned alternatives. Prior live forwarding basis linked; new boundary live proof owed. |
| R03 |Scenario/script/ABI-derived counts/signals, caller deadlines/envelopes, OS ports; no runtime/default/cleanup threshold added. |
| R04 |Explicit Control/Cancellation/Stage/progress variants, existing typed Fault/Terminal and owned lifecycle. Pure guards reject attempts/sends/expiry; actual transitions owed. |
| R05 |One existing worker/decoder/deadline seam; two-direction trusted gate plus independent receipt; no polls/cache/parallel manager. Wait mapping above is scoped implementation evidence, not a delegated audit. |
| R06 |One finite optional synchronization control and shared strict host case selection; no generalized flow engine. Root full source/evidence review owed. |
| R07 |Comments explain locally uncancellable barrier, no kernel-sleep observation and actual-receipt retention; corrected comment and final fresh checks recorded. |
| R08 |No4–5×/benchmark claim. Root equivalent-baseline distributions remain open. |
| R09 |New expected-no-effect contract, no reproduced/fixed defect. Real effect must be observed by root before any Deadline/worker fix. |
| R10 |Exact control-block/source/light/binary identities, logs, retained archives and replay/owed command above. Live mutation/restoration unrun. |
| R11 |amd64 GNU/musl pure/light only. Seven-target/actual hardware owed; macOS arm64 only, Windows ARM pause retained. |
| R12 |Existing native guest/socket/worker/child/cancel seam; no OS adapter, installed artifact, host root or trust/kernel/grant change. |
| R13 |One owned finite ingress boundary; original receive and ordinary contracts/assertions retained, shared host harness/helper; no actor authority/source-port trick. |
| R14 |No implementer audit/profile/benchmark. Parent/kernel/allocator/copy/fault/CPU/neighbor costs unmeasured, root owned. |
| R15 |Scoped implementation/pure checks, no children/heavy/live campaigns/main merge. Signed named-path checkpoint and owed root runs returned. |
| R16 |Same worktree/no local build,1CPU/1GiB/2GiB NEW private sources/targets; root caches/VMs/evidence preserved, no deletion/headroom bypass. |
