# One actual extra datagram at the held backend boundary

IMPLEMENTER `01a1c5d2ee0e7248982c95a59f1fd905`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Branch `p/managed-relay-extra-datagram` starts
at signed controlled-work-return checkpoint
`47912a11cf9faea9b7099cad8404671a67100e86`, retaining it and earlier receive,
strict200, notification `f8d643c59` and correction `cd5d7212` branches. Full
canonical AGENTS/R01–R16/evidence/CLAUDE/managed-plan/handoff readback preceded
edits. Policy files `f5cec365`, unchanged rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
The concrete proposal was sent before source with key
`relay-extra-datagram-proposal-v1` under the approved bounded fixture queue.

ROOT owns all actual eventfd/socket/namespace/cgroup/BPF/VM/failure/mutation
campaigns, audits, profiles/benchmarks, combined gates and main merges. No
subagents or local builds. This is new finite fixture coverage, not a product
routing/authorization change or an actual runtime result. The independent
pending-cancellation correction remains separate; ROOT composes its validated
source with these fixtures. No guessed production fix is added here.

## Exact case and one-use authority

```
sock-addr-extra4 {connected|unconnected} WORK_MS BUFFER_BYTES
```

The existing baseline Binary+Empty and one forwarded Binary reach the same held
backend decoder. This selected controller retains one native clone of the
**existing owned upstream socket**; ForwardOnly retains none. A consuming
FixtureInjection enum owns the clone, sends exactly one declared Binary request
under the original controller deadline, then drops it before waiting for work
return. No actor receives it. No source-port trick, reuseport, NAT, raw packets,
new grant, decoder, worker manager or general failure framework.

The controller first observes the independent third backend receipt/hold signal
and relay receive-stage entry. It then performs the one trusted fixture send.
FixtureExtraSend records actual send return, local/peer tuple and requested bytes,
explicitly separate from ForwardCompletion.upstream_sent. A send return still
does not prove delivery: the actual fourth backend receipt is mandatory.

The current held decoder consumes/counts/records BackendUnexpected with actual
bytes, peer and original tuple, then returns the existing typed UnexpectedDatagram.
It does not manufacture WouldBlock while readable. No new decoder behavior is
introduced. The finite ObserveExtra Work variant calls that same scripted()
function; only after it returns does the existing shared work-return helper
record Error/UnexpectedDatagram and signal completion. Notification/report failure
becomes typed I/O/error/expiry with consumed/sent facts retained; it cannot become
successful cancellation. The earlier actual receipt/work-return record, if
written, remains distinct from the later notification failure.

The parent waits for this actual work-return notification **before** requesting
relay cancellation. Cancellation cannot win first and hide the readable extra
datagram. Work return is not thread exit: records retain UnobservedUntilJoin and
the existing join supplies the actual joined terminal. Child reap → worker joins
→ every selected link detach → cgroup removal remains the owned cleanup order.
Original deadline, strict positive remaining time and normal error propagation
remain; no cleanup grace or replacement deadline.

Expected backend facts are three normal Backend receipts (Binary, Empty, held
Binary) plus one BackendUnexpected Binary: consumed4, validated3, one Empty and
two baseline replies. The fourth datagram is deliberately not validated as a
normal script step. The existing relay still reports ingress1/sent1/no reply;
the trusted injection is separately attributed. No cancellation rolls back any
receipt or kernel-accepted send. UDP supplies no reliable stream EOF, and missing
responses do not prove remote death or permanent nondelivery.

The host selected case requires exact fourth bytes `5245515545535400ff`, flags/
length/metadata, ordinal4, the same actual owned-upstream source and backend
original tuple, actual joined Error/UnexpectedDatagram with counts4/3/1/2,
work-return before cancel, forward Cancelled/UpstreamReceive/KernelAccepted and
actual cleanup/reaps. Sender log versus backend receipt order is not asserted:
the kernel send can complete before either thread wins the stdout lock. Only
causal same-worker/control ordering is required. Missing/extra/mismatched facts,
expiry, another I/O error, a forwarded response or failed cleanup cannot pass.

Original receive/pre-send predicates are byte-identical to strict200. Every old
case still rejects all UdpError/BackendUnexpected records; only this new explicit
case accepts exactly the specified typed error and independent consumed bytes.
Shared host parent/header/helper is untouched; ROOT wires the existing module.

## Sources, alternatives and bounds

[Controlled work-return evidence](../managed-relay-backend-end/README.md) and
[receive-cancel evidence](../managed-relay-cancel/README.md) retain the prior
proposal/source identities and primary
[Linux v6.18 eventfd](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/eventfd.c),
[poll](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/select.c),
[eventfd(2)](https://man7.org/linux/man-pages/man2/eventfd.2.html) and
[UDP contracts](https://man7.org/linux/man-pages/man7/udp.7.html). The original
forwarding record compares pinned Envoy/Cilium alternatives. Earlier ROOT actual
forwarding/receive and separate pending-cancel mutation evidence justify probing
this owned boundary; they do not prove the new extra-datagram case.

A new sender socket would introduce a different peer and obscure this specific
existing-flow boundary. Reusing the held upstream clone preserves its actual
source without granting the actor access. A separate manager/queue/channel,
sleep, repeated state probe or second decoder is unnecessary. Three one-shot
signals identify held receipt, receive entry and actual work return, each with
the original Deadline. Counts derive from Binary+Empty+held+one-extra; one uint64
per signal is ABI, not capacity policy. Envelope/deadline remain caller inputs,
ports OS-assigned. PayloadCapacityOnly still excludes allocator, parent, kernel,
console and descriptor costs; none is declared hard-bounded by the slot counter.

## Actual implementer validation

Own r12, Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`, rustc1.98.1, NEW
`~/managed-relay-extra-datagram-light-v1/source` and private guest/host targets,
Kiln1CPU/1GiB/2GiB admission. Policy/owned rustfmt0, GNU38/musl38 pure tests0, both
scoped production panic/unsafe clippy subsets0. Copied host composition with
actual shared parent `e1637959af1dbae631aa6f91c0d4d227f4c7dfac` and inert
type-matching common stubs compiles0; seven exact pure filters each1/pass1.
Ignored VM bodies compiled, never executed.

Pure additions reject missing/malformed arguments and a ForwardOnly injection
owner before any descriptor/I/O exists. Typed terminal/error tests reject cancel,
expiry, wrong stage, missing/extra validation and preserve consumed counters. No
eventfd/socket/namespace/cgroup/BPF/VM/profile/fault campaign ran. No full actual
normal helper/workspace/seven-target or supported hardware validation is claimed.

[light/](light/) retains all exact command/log/result JSON, Cargo locks/scaffolds,
source manifests/composition and three actual test-binary identities. All48 text
members/six final source paths were locally hash-verified. Transport14029B SHA256
`c3e4ea6c082365095a8c6a65e791cccc49fed6eedb7a2b17f7551ccf7f7311d3`;
compressed in-memory text only, no executable/local archive. Remote originals,
source/targets/binaries remain preserved. [source-identities.json](source-identities.json)
also proves original Deadline and both strict host predicates unchanged. No
runtime source changed after these checks.

Replay uses the existing source-linked scaffolds with NEW private output paths:

```
python3 docs/research/measurements/managed-relay-extra-datagram/light.py guest NEW_PRIVATE_GUEST
python3 docs/research/measurements/managed-relay-extra-datagram/light.py host NEW_PRIVATE_HOST
```

Local free space dipped below the floor during this work and ROOT restored it.
This lane has no active local target/cache, performed no local heavy build/full
worktree/archive/deletion and preserved all source/evidence/final artifacts,
protected ROOT Fedora/Mac targets and the paused Windows ARM VM.

## Owed ROOT actual gates

With explicit experimental kernel/config, original supplied budgets and exact
source-matching normal host/guest/helpers, ROOT runs:

```
SHARDS_SOCK_ADDR_MODE=connected SHARDS_SOCK_ADDR_BUFFER_BYTES=18 \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  cancel::direct_guest_udp4_extra_datagram_is_consumed_before_backend_error_and_relay_cancel \
  -- --ignored --exact --nocapture --test-threads=1
```

Repeat unconnected, fresh records.18 is the existing9-byte request+9-byte reply
slot envelope, not a runtime default. ROOT owes actual injected send plus fourth
independent receipt/error/join/cleanup, original error-versus-expiry, notification/
record/OS/worker/child failures, meaningful actual response/ownership guard effects
and exact restoration, original strict receive/pre-send and ordinary regressions,
combined seven-target/real supported hardware and added cost/parent/kernel gates.
No new actual effect or completion/performance guarantee is reported here.

## R01–R16 record

| Rule | Evidence / remaining gate |
| --- | --- |
| R01 | Fallible owned clone/send/record/signal/cleanup; existing checked consume counters and typed errors. No new runtime panic/indexing/arithmetic hazard; scoped GNU/musl denied lints0, OS/OOM failures unverified. |
| R02 | Proposal-first existing-seam decision, primary/established alternatives above and prior actual basis; new effects/cost remain ROOT owed. |
| R03 | One selected injection and script/ABI-derived counts/signals; no picked wait/default/authority increase. |
| R04 | Explicit FixtureInjection/Work/Cancellation variants, typed UnexpectedDatagram and work-return versus thread-join states. Pure negatives pass; actual transitions owed. |
| R05 | Current one worker/decoder/deadline seam, one-use owned clone/one-shot events; no repeated probe/cache/manager. ROOT audits actual waits. |
| R06 | Two real controlled-return callers share a small private helper; no generic failure framework. ROOT full source review owed. |
| R07 | Comments state ownership, notification scope and separate sender/receiver evidence; no EOF/kernel-return/rollback claim. |
| R08 | No comparative4–5×/performance result; ROOT measurements owed. |
| R09 | New finite coverage, not a guessed production fix. Actual failures require ROOT logs before any corrective change. |
| R10 | Exact source/light/commands/manifests/binaries retained; actual extra-datagram mutation/restoration remains unrun. |
| R11 | amd64 GNU/musl pure/light only; full seven-target/real supported VM gates owed, macOS ARM64 only and Windows ARM paused. |
| R12 | Existing native socket/worker/decoder only; no installed binary/OS adapter/host root/grant/kernel/trust/crypto change. |
| R13 | One finite extra request, no source-port trick or assertion relaxation; original strict predicates byte-identical. |
| R14 | No implementer audit/profile/benchmark; induced allocator/copy/fault/parent/kernel/neighbor costs unmeasured. |
| R15 | Scoped source/pure checks and signed named-path push; ROOT actual/combined gates/main merges, checkpoint is progress not lane closure. |
| R16 | Same worktree, NEW admitted remote sources/targets, no local heavy work or deletion, all protected ROOT/evidence/paused VM ownership preserved. |
