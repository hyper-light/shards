# Daemon registration and independent output credit

Implementer `/root/managed_output_endpoint`, policy SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
This is experimental P1/P3 implementation. The coordinator owns live E2E, audits,
resource accounting, failure campaigns, comparisons and the completion decision.

The daemon binds output before releasing a managed run, pins the initiating
identity, passes one private capture IPC endpoint last in the existing RUN FD
allowlist and supplies checked capacity/member metadata. The slim VM process
imports that endpoint without depending on remote/TLS. It sends canonical records
and receives scoped credit using the shared `abi::run::managed` codec. Normal
workload/control output never shares this bulk connection.

## Authority and lifetime

Run identity (`managed::RunId`) is distinct from an Ed25519 key fingerprint.
Subscription scope is run, declared member, stdout/stderr channel and generation.
For local requests, the owner key is the home's current crypto identity. A remote
request resolves its initiating, authenticated registered context key; an unknown
context refuses without a home-key fallback. The chosen key is retained as a
fingerprint independently of a mutable name. Active enrollment revocation still
needs the coordinator's integration gate.

Generation is a checked persisted u64 per container run ID, incremented and made
durable before release. Corrupt/truncated/exhausted counters refuse. A new run ID
has its own namespace. Snapshot restore and joins currently refuse managed scope
transfer, rather than reuse stale cursors; P11 must supply that contract.

Member numbering preserves the guest's declared order (agents, then harnesses),
including ordinal holes for non-runnable files-only declarations. Only the
image's authoritative runnable plan creates active output services. It is read
with existing EROFS `read_tree` and `build::vfs::Fs::stat`, preserving root clamp,
parent/final symlink resolution and Linux's40-link rule; only precise ENOENT or a
parsed config without `run` is inactive. Parse/resolution failures refuse. Data
reads at validated EROFS offsets are bounded by the guest config reader. The
O(image metadata) scan is cached with the verified rootfs/digest. Its metadata
allocation/high-water cost remains unmeasured; existing tree allocation limits
are not represented as proven hard admission by the output ledger. The guest
checks the packed active-ID mask before releasing actors. The
host validates every Open label against that member's trusted image label and
validates channel offsets/terminal transitions with the shared ABI state machine.

Each active member has two independent admitted channel slots and subscriptions.
A channel blocked without an observer does not take another channel's credit.
The private IPC reader routes a fixed prefix and checks the outstanding scoped
credit before allocating the remaining frame. A separate writer fairly rotates
ready channels. A disconnected channel retains at most its one previously issued
record, and can resubmit it when that same scoped observer returns. No global
`wait_capacity` holds sibling progress.

Authoritative run end/revoke closes the private IPC, cancels all of its output
services and wakes workers without waiting for an application ACK. Tenant abort
arrives separately on existing lifecycle control, revokes only that tenant's two
channels, discards their retained payload and marks their uncaptured tail unknown.
A previously granted late record can be drained and discarded without reopening
credit or ending siblings. Observer disconnect alone emits no abort and owns no VM
lifetime. Leases remain held until the actual worker/socket owners drop.

Canonical guest End/Aborted records are exact channel data. Owner-side abort
preserves accepted and application-consumed offsets in protected metadata and
ends transport; delivery of a separate terminal record through blocked data is
not claimed. Unknown uncaptured tails never become a clean End. Normal main EXIT
must not wait for subscribers. Those outcomes require root live validation.

## Typed outcomes

`managed::Error` distinguishes protocol, admission, closed and transport failures;
protocol/admission details are enums. `Reason` assigns checked stable application
close codes for protocol, scope, stale generation, closed and authentication.
Subscription scope contains the affected run/member/channel/generation. Producer
`Rejected` retains ownership on Full/Closed/Invalid. Daemon capture startup returns
`Failure` with Admission/Auth/Protocol/Transport/Endpoint/Closed variants, including
requested/available headroom in an admission refusal. The existing daemon run
boundary renders it to the legacy CLI text once. No branch compares error text.
Existing TLS/identity/platform error details are still wrapped from their legacy
error types; this slice does not claim that every legacy String is a typed wire
response. Application consumption, not a transport error's wording, decides credit.

## Admission and adaptation

A daemon ledger reserves from the host's reported available headroom minus all
outstanding capture leases. Unknown host readings, overflow, accounting failure
or insufficient headroom refuse. Already resident capture costs may be counted
again by the headroom reading; this is conservative. It is not hard cgroup/kernel
attribution or proof of every allocator/library/thread cost.

Before release, the ledger includes checked channel/state storage, actual
metadata-derived minimum records, two simultaneous host record owners (the VM
capture worker and the daemon), explicit QUIC in-flight/control/handshake/datagram
allowances, and queried UDP/private IPC queues. SO_SNDBUF/SO_RCVBUF and
GetNamedPipeInfo are cost facts after admission, never sources of authority.
Unknown queue sizes refuse. No full socket buffer is eagerly copied into a record.
The guest's shared scratch is inside VM memory; exact parent/producer attribution
is a separate G11 measurement gate, and is not proved by this ledger.

The TLS crypto ceiling comes from RFC8446's Handshake uint24 length plus four-byte
head (16,777,219 bytes). It is reserved, not eagerly allocated or claimed efficient.
Each record starts at the actual trusted Open label/terminal errno minimum.
`managed-capacity` is only the ABI protocol ceiling; each issued member/channel
Credit is the actual admitted amount. Before growth, the ledger reserves the
additional simultaneous record bytes, then raises the producer's allowance.
Growth never spends any sibling's already reserved minimum. The per-channel
ceiling and run aggregate are checked from the admitted active plan, not wire IDs.

`registry::Quantum` reuses the existing D226 Climb/Window/Welford method. Only
full Data records consumed by the application supply byte/time samples. Their
service includes grant, producer, queue and application ACK delay. Open/control/
terminal records and QUIC ACKs cannot manufacture throughput. Desired growth is a
request; refused reservation retains the old credit. Measured cold/warm/noisy/
degrading/stalled paths, fairness and retained-allocation cost remain root gates.

## Established alternatives and source boundaries

RFC9000 §§4/4.6 provides separate stream and connection flow control. A single
owner-authenticated listener with2N logical streams shares TLS/socket/thread costs,
but still requires tenant scheduling and connection-window admission. The current
functional composition reuses the same Driver/TLS for independent per-channel
endpoints, explicitly charging their extra costs. This is an experimental baseline,
not a measured architectural/performance preference. Root must compare equivalent
security/configurations before selecting the final composition or claiming4–5×.

- [Linux path_resolution(7)](https://man7.org/linux/man-pages/man7/path_resolution.7.html): root clamp, symlink traversal and40-link ceiling; existing build VFS resolver and its oracle tests are reused.
- [RFC9000 §4](https://www.rfc-editor.org/rfc/rfc9000.html#section-4): stream/connection credit.
- [RFC8446 §4](https://www.rfc-editor.org/rfc/rfc8446.html#section-4): uint24 Handshake length.
- [Linux socket(7)](https://man7.org/linux/man-pages/man7/socket.7.html): reported buffer bookkeeping.
- [Microsoft getsockopt](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-getsockopt): owned UDP queue queries.
- [Microsoft GetNamedPipeInfo](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-getnamedpipeinfo): actual pipe input/output sizes; zero can mean allocated as needed.
- [D226 source](../../../../crates/registry/src/climb.rs): existing online estimator and pinned primary references.
- [Relay alternatives](../../managed-runtime-buffered-relays.md): Envoy/HAProxy/QUIC comparison and accounting limitations.

No production panic is intended; parsing/arithmetic/new payload allocation are
checked and state/cleanup paths explicit. Scoped native checks cannot prove every
implicit allocation or library precondition. Existing telemetry remains evidence,
not tool-effect authority. No shell, host root, custom crypto, public ingress,
backend-socket handoff or extra installed binary is added.

All seven targets, actual KVM/HVF/WHP, mixed-agent/harness slow/noisy/closed channel
isolation, authorization/stale scopes, native TLS delivery, control under pressure,
partial I/O/failed workers, allocation/FD/handle return, full malformed/failure
campaigns, mutations and resource/latency/throughput comparisons are coordinator
work still owed. Light results and exact tested hashes are recorded separately;
older results do not certify a later source revision.

## Prototype limit: authoritative activation without a host scan

The current host classifier is a functional prototype. Its O(all image metadata)
walk, duplicate config parse and extra facts in the existing Agentfile READ map
are not justified as a final fast path. The cache does not prove freshness or
remove the scan's allocation/maintenance cost. No additional cache is introduced
or proposed to hide those costs.

A source-backed alternative starts at `init::run::Standby::start`, where
`domains::read()` already returns authoritative runnable domains before the command
is released (current `init/run.rs` around2590); `domains::start` runs later
(around232). Make managed control required, send a generation-scoped activation
list of stable declared IDs/kinds/labels over the reserved run-owned control/HELLO
path, validate it against the host's checked declarations, reserve active channels
and return a typed activation ACK/refusal before any main/domain actor is released.
Move the existing late fallback read ahead of untrusted launch. Owner cancellation
or stale ACK fails closed. This gate needs no observer/data credit, clock polling,
new cache or second host config parser. It is a sketch, not a new protocol already
implemented or a measured architectural decision. Root owns the comparison,
authentication/launch ordering evidence and final choice.
