# Finite two-leg buffered UDP4 comparison — TEST ONLY

Signed source **7ee6c4bcf90796c6a1540ed748a5ef89940825e1**, branch
`p/managed-network-buffered-relay`, starts at exact combined
`ebbda9b3b7b3de9a9143f4d383f60769ec7803b7`. The intervening main/combined update
was coordinator-reported code-identical; this branch preserves its approved base.
No production mechanism is selected or changed. Prior responder/presentation
checkpoints and the UDP-local tuple-reservation reds remain preserved.

Implementer renewed FULL canonical g121-main AGENTS, R01–R16, evidence, CLAUDE,
managed plan and current handoff before work and after compaction. Policy file
revision `f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
The coordinator owns architecture selection, evidence review, ALL real
socket/namespace/cgroup/BPF/VM execution, mutations/restoration, audits,
profiling/cost, heavy campaigns, seven-target/combined gates and merges.
This implementer executed only source-linked pure/light checks.

## Basis and narrow interface

The existing `Reply::Relay` worker generates a scripted response. It never
established payload forwarding. Coordinator reports for exact earlier e1bf source
(r511/r514) established fixed-original-peer presentation for this one known tuple
and meaningful hook-removal effects. Those observations justify comparing a
buffered forwarding path; they are not results for this new path. The existing
[presentation record](../PRESENTATION.md) retains its interface/source research.

The old command and its `Backend`/`Relay` child argv meanings remain:

```
sock-addr-udp4 MODE DEADLINE_MS PROFILE
```

New command has NO implicit profile or buffer-envelope default:

```
sock-addr-relay4 {connected|unconnected} DEADLINE_MS {destination-only|fixed-original-peer} BUFFER_BYTES
```

`BUFFER_BYTES` is a caller-supplied `usize` decimal envelope for the **sum of two
retained payload Vec capacities only**. The host requires positive
`SHARDS_SOCK_ADDR_BUFFER_BYTES`. The guest parses zero, but actual reservation
refuses an insufficient envelope before any child release, worker or hook.
Missing/malformed arguments are InvalidInput; the existing zero work-deadline
behavior remains TimedOut. No deadline is enlarged and no cleanup grace is added.

The controller still creates a NEW private network namespace, private mount
namespace/cgroup leaf, native loopback backend and separate native ingress on
OS-assigned ports. Both clients enter the held network namespace and their leaf
before fresh exec/socket creation; parent/control/backend/relay stay outside.
An additional held **native connected UDP socket** sends to the actual backend.
No actor receives that socket/FD, tuple repair, modified backend or cooperation.
The selected existing named unpinned BPF links and exact original-IP-AND-port
predicate are unchanged. Only the existing native UDP4 loopback destination is
selected; no source-port-preservation workaround is added.

Explicit ClientRole distinguishes BaselineBackend, ScriptedResponder and
ForwardedBackend. The first two preserve the old child argument spellings; only
the new comparison uses `ForwardedBackend`. Both baseline and comparison execute
the actual declared Binary then Empty script. A real backend receipt is required
for every exchange of BOTH scripts: expected counts derive from those two roles
and Exchange::SCRIPT, rather than an unexplained replacement count.

## Payload custody and state

`Slots::reserve` derives required initialized lengths from the actual maximum
request and backend-reply byte lengths in the finite script. It checks sum
overflow, reserves both vectors fallibly and checks their actual usable capacity
sum against the caller envelope. Allocator rounding can therefore cause a typed
Limit refusal; it never silently enlarges authority. Reservation and its
PayloadSlots record precede baseline child release. The Binary/Empty test cases
are semantic fixture inputs, not production datagram capacities.

The two slots are retained and reused for this worker's lifetime. Each exchange:

1. IngressReceive consumes an actual client datagram into the request slot.
2. RecordIngress emits length/flags/copied bytes/metadata BEFORE validation.
3. ValidateIngress checks script bytes, metadata and the first actual client
   tuple (Unbound→Bound; a different later tuple fails).
4. UpstreamSend sends the **received request slice** through the held connected
   socket; UpstreamReceive consumes the actual backend reply into the reply slot.
5. RecordReply emits actual reply observations BEFORE ValidateReply checks exact
   bytes, backend peer and the upstream socket's original destination metadata.
6. ClientSend sends the **received reply slice** to the actual client peer.
   ExchangeComplete is reached only after both legs, including an empty datagram.

UpstreamProgress is NotAttempted→Attempted only inside an actual send attempt,
then KernelAccepted after a full datagram return and ReplyValidated after actual
validated receive. ScriptComplete requires both actual exchanges. Report counters
record consumed/sent facts; they do not choose protocol state. Partial sends,
truncation/control truncation, unknown metadata, tuple/payload differences,
allocation/count overflow, OS errors and cancellation remain distinct typed
causes at the existing outer io presentation boundary.

ForwardCompletion reports joined ownership, last Exchange/Stage, upstream
progress, ingress/upstream/client counts, Empty counts and terminal outcome.
ForwardError preserves stage/cause/io-kind/errno. `external_commit` explicitly
remains unverified beyond independent backend receipts: successful send return
is kernel acceptance, not remote application commitment or rollback. An error
must not erase already observed consumed or sent facts. Report I/O can still fail;
no missing report is a passing fact.

There is ONE recvmsg/cmsg decoder, `udp_arrival_into`, taking borrowed initialized
mutable storage and returning reported length, flags, copied extent and typed
metadata. It retains that storage through the existing readiness/EAGAIN path.
`UdpView` lets the existing observation formatter read owned or borrowed storage
without a second decoder/trait. The native owning `udp_arrival` wrapper still
allocates once, calls the same decoder, truncates to actual copied bytes and
returns its original owned result. The strict `udp_receive`, metadata decoder,
address decoder and original endpoint predicates are byte-identical to base;
[unchanged-blocks.json](unchanged-blocks.json) records the exact ranges/hashes.
The decoder extraction preserves wire/error handling; it adds no unsafe
uninitialized storage optimization. Native/runtime equivalence remains a root
regression obligation, not established by this source comparison alone.

The concrete Work and Completion variants share the existing DatagramWorker
Running→Joined owner, eventfd cancellation, original absolute Deadline and join.
The old scripted worker keeps its explicit Binary→Empty→Binary backend loop and
Binary→Empty→ScriptComplete responder algorithm. No second worker manager,
periodic state probe, cache, generalized policy/map or growing flow history exists.

Both named links and held namespace/cgroup handles remain through child reap and
worker joins; every detach is attempted/reported as before. Cleanup preserves
observed join/detach/remove facts and nonzero errors. Early setup failures can
precede aggregate Cleanup/Observed; existing MountScope Drop attempts cleanup,
but this is not proof of successful removal. A still-pending owned cleanup is
bounded by the enclosing coordinator-owned VM lifetime, not by an invented grace
period or a claim that the work deadline proves all resources returned.

## Independent witnesses and honest scope

Host test `direct_guest_two_leg_udp4_records_buffers_and_backend_source_change`
is ignored and coordinator-only. It requires actual baseline and forwarded client
identities, original getpeername/receive endpoint, both Binary/Empty responses,
ForwardIngress and ForwardUpstreamReply bytes/metadata, and both backend scripts.
For forwarded backend receipts the actual peer MUST equal the held distinct
upstream socket and differ from the actor. BackendSource classifies an actor peer
as OriginalClient, not OwnedUpstream. Client-correct direct backend bytes alone
cannot pass. Actual upstream source CHANGE is recorded and required, not hidden
as native/server-side transparency. This comparator success, if later observed,
would establish finite transfer plus client presentation with an explicit backend
source difference, not full native end-to-end transparency.

Named Program endpoints, exact selected attach/detach records, actual child reaps,
worker completions and aggregate cleanup facts must all be present. Missing,
ambiguous, extra or failed facts remain failure. Destination-only is still an
explicit negative control, expected to fail the strict client presentation test;
a diagnostic driver accepting that red is not compatibility success.

PayloadSlots explicitly excludes allocator overhead, stacks, metadata, console
formatting, socket queues/kernel charge, BPF/FD/controller costs and child costs.
No total hard budget, allocation-free claim, measured overhead, 4–5× result,
per-tenant fairness, production relay, default-deny/bypass closure or P5 completion
is established. Multi-flow/destination, IPv6 forwarding, fragments/PMTU/QUIC,
FD transfer/inherited sockets, real forwarding topology, revoke/restart, hard
resource accounting, hostile actors and full route coverage remain OPEN.

## Sources and alternatives

[primary-identities.json](primary-identities.json) identifies retrieved primary
bytes; existing hook/UAPI sources remain pinned in the presentation record.

- [Linux udp(7)](https://man7.org/linux/man-pages/man7/udp.7.html): each receive
  returns one datagram; connect selects a default peer; truncation is explicit.
  No TCP stream/EOF/half-close or TCP containment predicate is imported.
- [recv(2)](https://man7.org/linux/man-pages/man2/recvmsg.2.html): an empty UDP
  datagram is consumed and returns zero; MSG_TRUNC/MSG_CTRUNC remain observations.
  [send(2)](https://man7.org/linux/man-pages/man2/sendmsg.2.html): send return alone
  is not delivery evidence. Both man-pages identities were captured at retrieval,
  not treated as the booted kernel version.
- [Linux v6.18 udp.c](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/udp.c#L1921-L2013)
  separates payload receive, peer presentation hook and ancillary construction.
  The existing single native decoder preserves those distinct kernel results.
- [Envoy v1.36 UDP proxy](https://github.com/envoyproxy/envoy/blob/v1.36.0/docs/root/configuration/listeners/udp_filters/udp_proxy.rst)
  forwards via actual upstream sessions; a nontransparent backend observes proxy
  source. Its transparent mode does not preserve upstream source port. Its
  sessions/defaults/timeouts are not copied into this one-flow comparison.
- [Cilium v1.18.0 bpf_sock.c](https://github.com/cilium/cilium/blob/v1.18.0/bpf/bpf_sock.c#L504-L578)
  uses real socket-cookie/endpoint reverse state for presentation, not payload
  custody. The existing fixed-known-tuple hooks are retained without such maps.
- Existing Shards `net::{Stack::flow,on_udp,poll,ring}` were inspected for the
  approved proposal. Pulling their full frame/flow/timer stack or TCP byte ring
  into this finite datagram fixture adds unrelated machinery. Reuse is instead
  at the already validated guest Deadline.io/eventfd/native UDP seam. The full
  production reuse/charging/route design remains coordinator work.

The two alternatives are preserved for comparison: the scripted responder lacks
forwarding evidence; direct backend access can return the right bytes but lacks
mediator custody. This implementation does not repair upstream source identity
with reuseport/NAT/raw packets/FD handoff or relax an original endpoint assertion.

## Actual light results and source freshness

Runner Fedora `8fadc1ec213332e16cb0d915a3fb913f`, admitted unchanged
**4 CPU / 1 GiB / 2 GiB**, rustc **1.98.1**
`48a229ceaefd4985c50990b14116b6d856af0985`, pinned libc0.2.189 and
serde_json1.0.151. Every attempt copied actual source into a NEW private directory
and used a NEW private target. No reused artifact mtimes, instrumented timing,
root target or local compiler was involved.

|Job / proof directories|Actual outcome|
|---|---|
|r143 / proof-v1|Toolchain0, GNU compile101 E0308: one old arrival formatter call still passed `&UdpArrival`; no test suite ran. Fixed only the missed `.view()` call.|
|r144 / proof-v2, host-v1|28 GNU and28 musl pure tests0, both scoped lints0; actual host caller extraction compiles0 and two old exact parser tests0.|
|r146 / proof-v3|GNU suite29passed/1failed: new test incorrectly expected InvalidInput for existing zero-deadline TimedOut. No musl/lint/host command after that failure. Corrected assertion ONLY.|
|r147 / proof-v4, host-v3|30 GNU and30 musl pure tests0; both scoped production panic/unsafe lint subsets0. Extracted actual host caller compiles0; three exact pure filters each run1/pass1.|

Final tests cover new required argv/malformed budget/profile/zero deadline,
legacy and new client roles, borrowed receive bounds/reported-versus-copied/error
metadata, Empty versus truncated datagrams, payload envelope/one-peer pinning,
and a pure backend-peer witness rejecting client-correct direct bypass. Existing
pinned wire encoding and prior state tests also pass. These tests never create
fixture sockets/namespaces/cgroups or load BPF.

The host extraction uses the actual entire host caller source and **inert,
type-matching common stubs**, including `Run.status: Option<i32>`. It compiles all
ignored test bodies but executes only the three named pure functions. It is NOT
the full actual common helper/CLI/guest build or a native/full workspace lint.
The GNU/musl lint commands deny warnings, unsafe-op and the recorded production
panic/unchecked-index/undocumented-unsafe subset; they are not scripts/lint.

Source SHA256s and base/commit identity: [source-identities.json](source-identities.json).
Final private guest/host source manifests match their signed source paths. Exact
commands/logs/results, manifests, Cargo locks, helper/common scaffolding and
seven actually built test-binary identities are under [light/](light/).
[light/transfer.json](light/transfer.json):10809B archive SHA256
`d9538bb83703399c91a74b98a3880d47250fb32a5568f3277751d8cb837f2b1a`,78 regular
entries/77 independently checked member hashes; no executable bytes transferred.
The original source snapshots/binaries/targets and archive remain in the named
private remote directories. `pack-light.py` reproduces this evidence selection.
No evidence/source/compiler cache was deleted. All own jobs are complete.

Local policy check, diff check and owned rustfmt1.98.1 check pass. Mac actual free
at renewed receipt was35504136KiB (above30GiB); no local build. Signing used gits
and named paths with the required attribution; source checkpoint is pushed.

Pure/light replay (NEW private directories only, same admitted envelope):

```sh
python3 docs/research/measurements/managed-network-sock-addr/light.py \
  "$HOME/managed-network-buffered-relay-proof-NEW"
python3 docs/research/measurements/managed-network-sock-addr/light-host.py \
  "$HOME/managed-network-buffered-relay-host-NEW"
```

## Coordinator invocation and owed gates — NOT RUN HERE

The coordinator supplies source-matching normal CLI/helpers/testguest and actual
experimental kernel/config association evidence. A separately supplied config is
input evidence; it is not independently verified as the booted effective config.
All current v4/v6/native/TCP/responder controls remain in source. The following is
one new finite case, with NO automatic envelope/profile/kernel defaults:

```sh
SHARDS_SOCK_ADDR_MODE=connected \
SHARDS_SOCK_ADDR_PRESENTATION=fixed-original-peer \
SHARDS_SOCK_ADDR_BUFFER_BYTES="$CALLER_PAYLOAD_CAPACITY_BYTES" \
SHARDS_KERNEL=/absolute/experimental/vmlinux \
SHARDS_TEST_KERNEL=/absolute/experimental/vmlinux \
SHARDS_NETWORK_EFFECTIVE_CONFIG=/absolute/experimental/config \
SHARDS_NETWORK_CONFIG_PROVENANCE='exact source/build/config association evidence' \
SHARDS_NETWORK_RECORDS=/absolute/fresh/records \
SHARDS_NETWORK_BOOT_MS="$CALLER_BOOT_MS" \
SHARDS_NETWORK_CASE_MS="$CALLER_CASE_MS" \
SHARDS_NETWORK_MEMORY_MIB="$CALLER_MEMORY_MIB" \
SHARDS_NETWORK_CPUS="$CALLER_CPUS" \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  direct_guest_two_leg_udp4_records_buffers_and_backend_source_change \
  -- --ignored --exact --nocapture
```

Repeat independently for unconnected and each explicit presentation profile.
Destination-only strict red remains a negative observation, never a success by
diagnostic-driver exit. Preserve raw `{name}.stdout/.stderr/.status.json`, kernel/
config/guest identity, actual cmdline and `.payload-input.json` before assertions.
Build identities, guard admission and original source restoration must remain
explicit. The experiment adds no production kernel pin/config/grants/settings.

Owed root validation includes:

- Full actual host/guest/helper compile; existing four responder controls, native
  decoder/socket regressions, peer UDP/TCP cases and relevant supported matrix.
- Actual connected/unconnected new cases and both profiles: baseline liveness,
  Binary/Empty datagrams, original client presentation, actual backend source
  CHANGE, independent ingress/upstream/client/backend byte and tuple witnesses.
- Relevant destination-hook removal/restoration: correct BACKEND bytes obtained
  directly must FAIL on missing relay custody/backend peer equal actor. Keep
  presentation-hook removal/restoration effects distinct from routing custody.
- Meaningful retained request/reply-buffer payload mutations after receive:
  demand actual changed backend/client bytes, preserving observations and exact
  restoration. A missing event/log alone does not prove a buffered byte guard.
- Actual below-required/allocator-rounding refusal, allocation/error/record failure,
  consumed-but-send-failed and uncertain commit stages, cancellation at each leg,
  stalled/dead backend/client/worker, all child reap/worker join/link detach/leaf
  removal outcomes. Do not infer cleanup success merely from Drop or VM exit.
- Coordinator-owned resource/cost accounting, failure/death/revoke/bypass tests,
  seven-target and KVM/HVF/WHP coverage as applicable. Paused Windows ARM stays
  paused; unavailable hardware and unchanged broader gaps are unverified.

These are owed commands/experiments, not passed checks. No P4/P5/full-route or
managed-plan completion claim follows from preparing this source.

## R01–R16 delta record

|Rule|Applicable evidence and remaining limit|
|---|---|
|R01|Fallible initialized slot allocation and actual capacity/sum checks, checked slices/counts, Builder thread spawn, existing panic lints retained; pure GNU/musl scoped lints0. Actual allocation/OOM/OS/poison/cleanup failures remain root gates; no panic-free proof from lints alone.|
|R02|Primary Linux/man-page contracts, pinned Envoy/Cilium alternatives and existing Shards seams inspected for the accepted finite proposal. Prior live hook results are coordinator reports for their revision; actual new transfer/cost evidence is owed. No product architecture chosen.|
|R03|Two slots/two exchanges derive from concrete request/reply Binary+Empty script; ABI lengths/flags remain protocol facts, ports OS-assigned, capacity/deadline supplied. No upstream default/resource threshold copied or arbitrary replacement cap. Allocator rounding/overflow refuse.|
|R04|Explicit controller/client/work/completion variants and per-leg stage/progress/peer states; typed Fault/ReserveError, facts before validation, exact old original tuple predicate. Source/pure transitions checked; actual partial-effect/cleanup/error ordering owed.|
|R05|One native decoder and one DatagramWorker eventfd/readiness/original Deadline owner serve real scripted+forwarded callers. No cache/history/polling/parallel manager/trait framework. Actual cancellation/retention/effect evidence remains coordinator work.|
|R06|Finite straight-line leg state sequence, shared worker lifecycle, no generalized flow engine/threshold. Added code/branch combinations require coordinator source review; pure checks do not certify every path.|
|R07|Comments explain borrowed initialized custody, capacity scope and byte-order/safety obligations; the forward submodule and this record distinguish the finite comparison from production routing.|
|R08|4–5× comparative target remains unmeasured; no ratio/distribution/performance success claim or weakened contract.|
|R09|This is an authorized new finite mechanism comparison, not a claimed product bug fix. Earlier live presentation basis and both new compiler/test reds retained; new runtime issues must be reproduced by root before remedies.|
|R10|Signed source, exact base/file hashes, primary hashes, copied-source freshness, durable compiler reds/greens, binary identities, replay/owed effects all retained. New before/after and meaningful live mutations unrun here.|
|R11|Light Linux amd64 GNU/musl only, not seven targets or VM hardware pass. macOS arm64 only; Windows/Linux amd64+arm64 GNU/musl obligations retained; Windows ARM pause preserved.|
|R12|Existing guest native socket/Deadline/worker/emitter/hook/mount/child mechanisms reused; no product platform duplication/new installed artifact, host root or crypto change. Full product seam/resource reuse is not selected by this fixture.|
|R13|Concrete one-flow worker and explicit ClientRole required by actual script semantics; no broad policy/relay framework, fallback/source-port workaround or extra default protocol. Existing controls remain replayable.|
|R14|No implementer audit/profile/benchmark; actual allocations/copies/retention/page-fault/CPU/kernel/console and neighbor costs remain root-owned/unmeasured. Payload-capacity record is not total resource accounting.|
|R15|Implementer owns only approved source/fixture/docs and targeted pure/light checks. No children, sockets/BPF/ns/guests/mutations/heavy campaigns/root cache writes or main merge. Coordinator receives signed checkpoint and all owed gates.|
|R16|Unchanged4CPU/1GiB/2GiB admission, NEW private sources/targets, Mac floor30GiB/no local compiler, root caches/paused VM untouched. Sources/evidence/binaries retained; no recovery deletion or headroom bypass.|
