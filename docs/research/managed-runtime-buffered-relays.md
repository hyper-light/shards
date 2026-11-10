# Managed runtimes: buffered output and network relays

Research date: 2026-11-09. Shards revision examined: `ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8`. This is a research and design input, not an implementation or an E2E pass. **Observed** means source read at that revision; **fact** means supported by the linked primary reference; **proposal** means a recommended Shards design; **open** means a decision or verification remains. The coordinator owns the final code audit and formal research record `01a1c33a30dd72afa53e3cbda62339cb`.

## 1. Recommended contract

**Proposal.** An `AGENT` or `HARNESS` declaration creates an owned runtime, with its declared executable and literal argument vector started by Shards' native supervisor. Before execution, the supervisor installs private capture endpoints for file descriptors 1 and 2 and the runtime's network confinement. An untrusted runtime, including all descendants, never receives the broker's external connection, management channel, or output-consumer authority. Installing the artifact, capturing output, and starting it require no user shell pipeline.

Every permitted external or internal communication crosses a Shards-owned release point. That release point authenticates its control plane, associates the flow with a trusted runtime identity and grant, owns or retains its bytes/datagrams, charges the corresponding resources, and applies the required policy before forwarding. No successful grant means unrestricted forwarding after that check: revocation, cancellation, memory pressure, and queue state remain part of the flow's lifetime.

**Interpretation of mandatory buffering.** Require actual ownership, accounting, policy mediation, and retention of an unwritten tail. Do not require a redundant allocation, an extra copy, or a timer delaying an otherwise ready write. A buffer is allowed to be an already owned input frame, a charged pipe, or a managed kernel buffer, provided that its lifetime and release are controlled. A direct socket handed to an untrusted child is a bypass. An immediate checked write from a broker-owned frame need not be one.

**Limit.** A finite buffer cannot absorb an indefinitely faster producer without eventually stopping that producer, using additional bounded storage, or reporting loss/failure. Mandatory interception does not make infinite storage possible. The runtime must specify those outcomes rather than silently lose data or consume all host memory.

## 2. Existing Shards seams and concrete gaps

These are observations from the examined tree, not claims about an unexamined later tip.

| Existing seam | Observed behavior | Consequence for the new contract |
| --- | --- | --- |
| [`init/domains.rs`](../../crates/init/src/domains.rs), `Started`, `start`, `child` | A domain has one nonblocking read pipe. The child duplicates its single write end onto both stdout and stderr. The clone/setup path already owns namespaces, cgroup, UID, descriptor closure, and confinement. | Install two capture endpoints before exec here. Keep separate channel identities; a new wrapper command is unnecessary. |
| [`init/run.rs`](../../crates/init/src/run.rs), `Owner::Domain` | Domain reads go through `frames::prefixed_lines` and enter the main run's `STDERR` stream. Read interest follows a shared `to_host` occupancy bound. | Current behavior mixes workloads and can couple their backpressure. A dedicated managed-output stream needs per-source accounting and scheduling. |
| [`init/frames.rs`](../../crates/init/src/frames.rs) | `prefixed_lines` inserts a label, splits long lines, and adds a newline to unfinished output; `Outbox` already retains partial writes with a cursor. | Preserve raw bytes first. Line prefixes belong in an optional display adapter, not the canonical capture record or a tool protocol. Reuse the partial-write discipline. |
| [`abi/run.rs`](../../crates/abi/src/run.rs) | Existing frame header identifies stdout/stderr and payload length, not runtime/process/tool identity. A separate signal port prevents blocked stdin from holding signals. | Preserve Docker/stdcopy compatibility. Add a versioned managed-stream envelope or separate stream, and retain independent cancellation/control progress. |
| [`net/tcp.rs`](../../crates/net/src/tcp.rs) | Shards terminates guest TCP, owns per-flow `to_host`/`to_guest` rings, handles short writes and a drained send-side shutdown. Current ceilings are fixed `TO_HOST`/`TO_GUEST` values. | This is a useful relay engine, not proof that the new budget and audit requirements are met. Introduce aggregate credits and derived capacities. |
| [`net/poll.rs`](../../crates/net/src/poll.rs), [`net/ring.rs`](../../crates/net/src/ring.rs) | Cross-host readiness adapter; page-backed fallible rings whose mappings can be returned to the OS. | Extend existing facilities rather than adding a thread and independent event loop per flow. Count allocated/retained pages, not just logical queued bytes. |
| [`net/lib.rs`](../../crates/net/src/lib.rs) | Datagram-like guest-bound backlog drops its oldest entry at a fixed bound. UDP socket sends discard their return result in the examined path. | Explicitly count and expose congestion/error/drop outcomes. Do not advertise lossless UDP or silent best-effort telemetry as complete capture. |
| [`init/links.rs`](../../crates/init/src/links.rs), `Switch`/`Gate` | Domain-to-domain traffic is routed through guest network namespaces with nftables grants and isolated connection tracking. The switch is not a userspace byte relay. | Extending the host Internet relay alone misses internal domains, engine members, and IPv6 routes. Forced guest-side mediation is a necessary additional seam. |
| [`init/netplan.rs`](../../crates/init/src/netplan.rs) | Network membership and directed flow grants are separate. | An output declaration must not manufacture a network grant, observer right, or ingress exposure. |
| [`shards/daemon/ship/stream.rs`](../../crates/shards/src/daemon/ship/stream.rs) | Existing logging holds messages, probes refusals, and distinguishes some application protocols' acknowledgements. | Reuse applicable machinery selectively. Its RTO-based receiver heuristics are not a universal application-delivery proof for arbitrary tool/network streams. |
| [`remote/net.rs`](../../crates/remote/src/net.rs) | Existing remote transport has a driven connection and stream-level consumption. | Cross-host output can retain broker ownership and propagate credits through the existing remote transport. Do not transfer an external socket to the runtime. |

### The existing TCP fast path, traced

`net::take_frames` copies a validated shared-netring frame into the network process's own scratch buffer. `on_guest_frame` checks framing and source MAC; `on_guest_ip` checks source addressing. A newly opened external TCP flow passes the configured destination grant/proxy/resolver checks before `Conn` is created. `Conn::on_segment` handles stream sequence/window acceptance and writes immediately only when no earlier bytes wait. `write_now` advances across short writes, retries interruption, and stops at `WouldBlock`; the unsent suffix enters `to_host` before `rcv_nxt` advances.

**Inference.** That particular optimization has a mediator-owned input lifetime and an unwritten-tail discipline. Requiring another queue merely to call it buffered would add work without providing isolation. It still needs the proposed aggregate credit/policy release gate and metadata accounting before forwarding. Also, `on_guest_ip` has a peer-forwarding branch before the external destination-policy path, by the existing bridge design. That branch and kernel-routed intra-VM traffic must appear in the coverage inventory; neither should be assumed covered by the external TCP checks.

## 3. Transport facts the relay must preserve

**Fact.** TCP provides a bidirectional ordered byte stream; application write calls and TCP segments do not supply application message boundaries. A send-side close does not imply that the receive side should be discarded. Retain a separate state and drain point for each direction. [RFC 9293, §§2.2, 3.6.1, 3.7, 3.9.1.4](https://www.rfc-editor.org/rfc/rfc9293.html)

**Fact.** A successful `send` is not an application-delivery acknowledgement. Nonblocking sends may take only some data or report that they would block. A relay cannot infer that a tool executed, a log receiver stored a record, or an HTTP server committed a request because the local write or remote TCP ACK succeeded. [Linux `send(2)`](https://man7.org/linux/man-pages/man2/send.2.html)

**Fact.** UDP preserves datagrams rather than stream records, and does not guarantee delivery, uniqueness, or order. The receiving API can truncate a datagram if given insufficient space. UDP applications and tunnels also need congestion-aware behavior. Preserve boundaries and destination metadata, detect truncation, and distinguish local queue admission from remote application receipt. [RFC 768](https://www.rfc-editor.org/rfc/rfc768.html), [Linux `udp(7)`](https://man7.org/linux/man-pages/man7/udp.7.html), [RFC 8085](https://www.rfc-editor.org/rfc/rfc8085.html)

**Fact.** An HTTP CONNECT tunnel is a blind relay after successful establishment, with an explicit destination host and port. It is not permission to connect to every destination. An opaque TCP relay or authorized CONNECT tunnel can retain TLS end to end. [RFC 9110, §§3.7, 9.3.6](https://www.rfc-editor.org/rfc/rfc9110.html)

**Fact.** TLS protects confidentiality and integrity; TLS 1.3 closure alerts are themselves encrypted. A transport close before `close_notify` does not establish that all sent TLS data arrived. An opaque relay cannot validate the encrypted application conversation or synthesize a genuine peer's TLS closure. [RFC 8446, §§1, 5, 6.1](https://www.rfc-editor.org/rfc/rfc8446.html)

**Proposal.** The default network relay forwards ciphertext unchanged and records endpoint/identity/grant/byte-count/queue/error metadata. It does not insert a CA, disable certificate checks, extract credentials, or rewrite encrypted payloads. If a rule needs the meaning of a tool request, enforce it at an explicitly integrated tool/protocol boundary before encryption. When that boundary is unavailable, label semantic visibility unavailable. Passive transport observation cannot establish universal semantic visibility for malicious, encrypted, custom, or multiplexed protocols.

## 4. Ownership and typed state

**Proposal.** Give each mediated flow a supervisor-issued identity and a typed state machine, not a caller-supplied string tag:

```text
Owner identity + grant revision + flow identity + direction
  -> admitted transport with charged resources
  -> owned input bytes or datagram
  -> policy-approved release
  -> partial writes / downstream consumption
  -> drained direction / reset / explicit failure
```

The identity must bind to the stable runtime/cgroup and supervisor generation. An untrusted descendant's PID, argv text, output prefix, claimed JSON `tool_id`, destination string, or in-band event is evidence from that descendant, not an authorization principal. A new subagent cannot obtain another root runtime's budget by choosing another name or opening many streams.

Represent stream EOF, datagram boundaries, producer exit, consumer disconnect, cancellation, policy refusal, and quota exhaustion as different events. Model read and write closure independently. A TCP FIN waits behind already accepted bytes on that direction; peer reset and local policy abort remain explicit. Cleanup removes readiness registrations before closing/reusing descriptors and invalidates stale tokens.

**Fact.** Linux's epoll documentation explicitly covers readiness starvation and stale cached events. Round-robin readiness work avoids allowing one continually ready descriptor to consume an entire loop. Shards already uses a cross-platform level-triggered adapter; adopting edge-triggering would additionally require nonblocking drain-to-`EAGAIN` discipline. [Linux `epoll(7)`, “Possible pitfalls and ways to avoid them”](https://man7.org/linux/man-pages/man7/epoll.7.html)

**Open.** Exact typed API and module placement require the coordinator's design audit. A suitable seam is a narrow relay core owning credits, queue cursors, source identity, closure and scheduling; platform adapters own polling/socket primitives; protocol adapters own optional semantic validation. Keep application policy distinct from operating-system I/O mechanics.

## 5. Private stdout/stderr capture and attribution limits

**Proposal.** Before the native child execs, install separate private stdout/stderr pipes, close all unrelated write ends, and keep the reads in the supervisor. Drain the two channels independently and export source/channel records over the dedicated output service. Do not redirect them into the main command's raw stderr. Inherited descriptors capture descendants that continue to use them, at any depth.

**Fact.** A pipe is a byte stream, not a writer-tagged message channel. EOF occurs only after every write descriptor is closed, including inherited copies; fullness blocks writers or produces `EAGAIN` in nonblocking mode. Linux pipe capacity is queryable and must not be treated as a portable fixed size. [Linux `pipe(7)`](https://man7.org/linux/man-pages/man7/pipe.7.html)

**Inference.** A shared inherited stdout pipe proves the owning runtime and the stdout channel, but not which descendant wrote a particular byte. Two processes can write into it, splice writes together, or deliberately produce misleading tool-shaped text. Cross-channel ordering is also only the supervisor's observation order, not a recovered global causal order.

**Proposal.** Canonical records contain trusted supervisor generation/runtime/source class/channel/stream offset, optional verified process identity, and an opaque byte payload. Process or tool attribution has an explicit evidence level: verified launch/connector correlation, runtime-only, or untrusted application claim. Exact per-process attribution requires a controlled launch/capture boundary or separately validated kernel/protocol correlation; a line prefix is insufficient.

**Coverage limit.** A child may replace FD 1/2, write to a granted file or Unix socket, use shared memory, or put data on another permitted port. Capture of the inherited pipes alone does not cover those operations. Network confinement, descriptor-right checks, filesystem/tool mediation, and the process-monitoring research must account for alternative channels. If an operation is outside the declared mediated surface, deny it or truthfully report it as outside coverage; do not call its content captured.

**Proposal.** Keep byte capture separate from display sanitization and semantic tool parsing. Do not convert malformed UTF-8, NULs, ANSI escape sequences, arbitrary binary bytes, or partial lines into a different canonical stream. Safe terminal/UI rendering can escape controls; an authorized raw export can reproduce bytes explicitly. Displayed source labels come from trusted metadata, never an output line that imitates a control record. Payload bytes never authorize a signal, a grant change, another subscriber, or a telemetry identity.

## 6. Output framing and ports

**Proposal.** Prefer a versioned, length-delimited binary-safe envelope built on Shards' existing frame discipline. Keep raw bytes as bytes, encode source/channel/offset in fixed typed metadata, reject unsupported versions, check all arithmetic and negotiated record bounds before allocation, and permit incremental parsing across arbitrary transport splits. A bounded frame may contain only part of an application's line/message; do not wait for an entire unbounded line to forward it.

Two standards are useful comparisons, not dependencies that must be added. CBOR distinguishes byte strings from UTF-8 text and can carry typed metadata without base64; JSON text sequences provide record separators and incremental recovery, but encode UTF-8 JSON rather than arbitrary raw binary. Shards can retain its existing small framing machinery if it supplies the same necessary checks. [RFC 8949, §§3.1, 5, 10](https://www.rfc-editor.org/rfc/rfc8949.html), [RFC 7464, §2](https://www.rfc-editor.org/rfc/rfc7464.html)

**Open.** The output service's listening side, wire format, and fixed default number have not been selected by the user. A possible default is an owner-only logical VM output service, carried over the existing host/guest channel, with a host listener allocated by the OS (`auto`) when a TCP endpoint is requested. Report the allocated endpoint through trusted control metadata, exclude 80/443 as requested, and allow explicit published mappings under existing `EXPOSE`/`NETWORK` rules. This avoids choosing an unsupported magic port. Do not treat this proposal as an already approved fixed-port specification.

**Fact.** VSOCK uses CID plus a 32-bit service port and is independent of virtual IP networking; stream connections can disconnect on VM migration. A TCP destination port, a host listening port, and a VSOCK service port are distinct address spaces/roles. [Linux `vsock(7)`](https://man7.org/linux/man-pages/man7/vsock.7.html)

**Fact.** Squid documents 3128 as a usual proxy listening port, while its current directive page lists no inherent default configuration value. That is a proxy-service convention, not a requirement that a new output service use 3128 or 8080. [Squid `http_port`](https://www.squid-cache.org/Doc/config/http_port/)

**Proposal.** Keep output delivery, HTTP egress proxying, and application HTTPS destinations separate. Listening away from 80/443 honors the requested operational choice; it does not grant outbound HTTPS. Authenticate output subscription before releasing data, scope it to the run/runtime and requested channels, and revoke it with the owner/session. Loopback binding or an unguessable port alone is not authorization. A guest-controlled process must not subscribe to or issue control commands to its own supervisor simply because it knows the service number.

## 7. Bounded memory, fairness, and adaptive work

**Proposal.** Use a hierarchy of hard credits: supervisor/service budget, owning runtime budget, flow budget, and direction/subscriber budget. Charge actual allocated/retained queue pages, parser/frame state, kernel sockets/pipes, datagram descriptors, replay/spool indexes, and outstanding control requests. Opening another flow or descendant consumes the same parent runtime's allocation. Refuse admission before descriptor or memory exhaustion and ensure accepted work can still release resources.

For a direction ready to read, the permissible read is bounded by available charged space and the destination's current credits. Disable its read interest when no room remains and wake it on released credits rather than a periodic arbitrary retry. Preserve queued offsets after short writes; an independent blockage reason must not be cleared when another reason resolves. Consumers receive credit only for the semantics they actually acknowledged, not merely because another buffer accepted the same bytes.

**Fact.** Linux reports socket buffer settings, but `SO_SNDBUF`/`SO_RCVBUF` accounting includes implementation overhead; the reported send value is doubled for bookkeeping. These values are inputs to an implementation-specific budget, not a portable exact count of application bytes available. [Linux `socket(7)`](https://man7.org/linux/man-pages/man7/socket.7.html)

**Fact.** cgroup v2 exposes total descendant memory and network-buffer memory. `memory.high` throttles/reclaims, whereas unrecoverable pressure at `memory.max` can invoke the cgroup OOM killer; these are different mechanisms. [Kernel cgroup v2 documentation, memory interface and `memory.stat`](https://docs.kernel.org/admin-guide/cgroup-v2.html)

**Inference/proposal.** Parent-owned relay pages may not be charged as the child producer's memory. Maintain logical producer charges and an independent relay budget, in addition to the guest's cgroup limits. Measure actual attribution on the pinned kernel and all host backends. A process with permission to create many cheap descendants must not force unlimited expensive parent allocations or hold a global lock while its slow operation completes.

Capacity sources must be explicit: run/user memory and disk budgets, current host headroom, OS page/pipe/socket information, negotiated transport/window bounds, descriptor limits less held/reserved descriptors, and measured producer/consumer progress. Protocol maximums bound parsing; they are not preallocation targets. An OS socket buffer alone is not a complete resource policy. The design must still work at the minimum admitted capacity.

Batching needs no flush timer merely to collect output. Process ready bytes up to owned credits, and flush whenever the sink can accept them. For throughput, grow work within available credits when measured service improves; reduce speculative queueing when latency/pressure rises. A bandwidth-delay estimate is a performance input, never permission to exceed the aggregate budget. Record estimator inputs and failure modes. No measurement from one machine becomes a fixed production buffer number.

Fairness must be hierarchical by owner, then flow/channel—not solely by socket count, which an attacker can multiply. Maintain a ready list, rotate after bounded measured/negotiated work, and wake blocked owners when credits return. The work quantum and refill policy need an explicit derivation and starvation tests; copying a proxy's conventional buffer size or “half full” ratio is not one. Independent cancellation, policy revocation, and completion records require reserved capacity derived from the admitted state/control record requirements.

## 8. Reference architectures and multiplexing

**Fact.** Envoy propagates buffer pressure upstream by disabling reads or ceasing HTTP/2 consumption/window updates. Multiple independent blockages require matching unblock accounting. Its watermark bounds are soft because already permitted data can still arrive. This is useful evidence for credit propagation; its conventional low watermark is not a Shards production constant. [Envoy flow-control implementation notes](https://github.com/envoyproxy/envoy/blob/main/source/docs/flow_control.md)

**Fact.** Envoy distinguishes streaming filters from filters that require a whole body. Whole-body buffering adds a request/response size limit and an explicit failure outcome; streaming does not require retaining an entire body. [Envoy flow-control FAQ](https://www.envoyproxy.io/docs/envoy/latest/faq/configuration/flow_control)

**Fact.** HAProxy documents a process-wide buffer limit and emergency buffer reserves, and warns that sustained allocation shortage can stall some tasks. This is a useful negative lesson: a nominal global cap without progress/reserve invariants is insufficient. Do not copy its buffer sizes or reserve count. [HAProxy 3.2 configuration, `tune.buffers.limit` and `tune.buffers.reserve`](https://docs.haproxy.org/3.2/configuration.html#tune.buffers.limit)

**Fact.** Envoy's overload manager protects the proxy's own memory/CPU/descriptors separately from upstream service circuit breaking. Shards likewise needs relay survival under aggregate malicious load, not just one flow's queue ceiling. [Envoy overload-manager architecture](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/operations/overload_manager)

**Fact.** HTTP/2 has connection and stream flow-control windows; stream credit alone cannot eliminate connection-wide interference. WebSocket control frames can occur inside fragmented messages, and unaware intermediaries must not arbitrarily rewrite negotiated framing/extensions. [RFC 9113, §§5.2, 6.9](https://www.rfc-editor.org/rfc/rfc9113.html), [RFC 6455, §§5.4–5.5](https://www.rfc-editor.org/rfc/rfc6455.html)

**Proposal.** Treat connection, logical stream, and owning runtime budgets as different layers. Keep Shards management off a user data channel whose receiver may deliberately stop consuming. An opaque encrypted relay preserves bytes and transport semantics; a protocol-aware authorized adapter must understand the full framing/extensions it changes. A relay should not buffer an entire indefinite HTTP body/WebSocket message just to call it intercepted. A semantic rule requiring the complete object needs a derived or user-imposed object budget and a defined refusal path.

**Optimization option.** Linux `splice` can move data through a pipe without a userspace copy. It is not a cross-platform baseline, its flags do not alone guarantee nonblocking behavior, and a content policy still needs access to what it validates. Measure a broker-owned pipe optimization only after the same ownership/credit/policy checks; do not substitute it for them. [Linux `splice(2)`](https://man7.org/linux/man-pages/man2/splice.2.html)

## 9. Slow consumers, fanout, restart, and failure policy

**Proposal.** State the policy per surface rather than making a universal “delivered” claim:

| Surface | Recommended behavior when budget or consumer capacity runs out | Required observable result |
| --- | --- | --- |
| Canonical runtime stdout/stderr | Apply backpressure to its private capture; optionally append to owner-authorized quota-bounded spool. Never silently discard canonical bytes. | Queue/spool state and blocked producer; explicit terminal error if continuation is impossible. |
| Optional live observer | Independent cursor and bounded lag; detach or report a gap when its allowance is exceeded. It does not stall unrelated owner streams. | Trusted gap/offset range or disconnection reason. |
| Required audit sink | Reserve bounded local durability or stop admitting/releasing operations that require that audit record. | Quota/failure state; no false claim that monitoring stayed complete. |
| TCP/Unix byte relay | Backpressure that direction; drain accepted bytes before a normal half-close. Cancellation/refusal can explicitly abort. | Distinguish accepted, queued, written, peer-stack-acknowledged where measurable, and application-acknowledged where the protocol supplies it. |
| UDP relay | Preserve each datagram; bounded admission with explicit drop/refusal metrics and congestion policy. | No success claim beyond actual observed semantics; count local truncation/send failures/queue drops. |
| Credentials or sensitive network bodies | Avoid persistence by default; capture operational metadata unless the owner expressly grants content storage. | Redaction/storage scope is explicit; raw retention never appears merely because telemetry is enabled. |

Backpressure is an intentional behavior change: a runtime writing endless output to a stopped consumer can block. Control/cancellation must still work. Closing or killing only the original process is not proof that capture is drained; descendants can keep write ends open. Lifecycle policy owns the subtree, its capture handles, and the final drain outcome.

Fanout should keep one immutable payload with independently charged subscriber cursors, not multiply unbounded copies. A new subscriber receives only authorized history; its chosen resume cursor is checked against the retained interval. `Gap`, `End`, and `Aborted` are trusted supervisor records distinct from application bytes. A malicious observer cannot acknowledge another observer's cursor, producer, run, or generation.

Reconnect can replay explicitly retained managed-output records by generation/stream/offset. The consumer must deduplicate or acknowledge its committed cursor if stronger semantics are required. Reconnecting a raw external TCP conversation does **not** permit replaying arbitrary requests: a previous request may have committed before the disconnect. Preserve reset/failure and leave retry/idempotency decisions to a protocol that defines them.

Spool storage needs an owner grant, encryption/access controls appropriate to contents, charged disk/page-cache/index space, crash-consistent cursor semantics, and quota exhaustion behavior. No endless disk growth or deletion of unrelated user data. If that storage is unavailable, block or fail the declared operation explicitly. Exactly-once application execution is not obtained from transport reconnection.

## 10. Coverage and adversarial verification

Before claiming all controlled ports are intercepted, build an executable route inventory: external TCP/UDP IPv4/IPv6, published ingress, domain-to-domain and engine-member traffic, same-domain loopback, DNS, AF_UNIX/path sockets, VSOCK services, remote transport, inherited/received descriptors, and any granted device or alternative network mechanism. Assign every permitted edge a broker release point; make unassigned edges unreachable. Preserve current source-address validation and explicit network grants. Packet filters that only permit a direct route are not themselves the required byte-buffered relay.

The process-security study owns the syscall/FD/capability threat model. This relay design requires its conclusions: untrusted runtimes cannot inherit management sockets, import raw external sockets through FD passing, create alternate privileged network paths, mount another runtime's endpoints, rewrite trusted source metadata, or exhaust the supervisor before admission accounting. A passive sensor may observe a bypass after the fact; that is not synchronous mediation.

Focused tests should use actual capacities or explicit test budgets rather than sleeps that assume an unloaded host:

| Test | Failure it must detect / useful mutation |
| --- | --- |
| Start a declared harness with separate stdout/stderr, binary bytes, no newline, split UTF-8 and long records. | Canonical equality and correct channel; remove capture setup or restore `prefixed_lines` and the test fails. |
| Descendant fork/exec tree retains one writer after parent exit; then closes it. | No premature EOF/end; remove writer-lifetime accounting and the test fails. Do not invent per-PID attribution from its shared pipe. |
| Slow or stopped owner/observer, endless producers, sibling quiet control workload. | Bounded allocated memory, enforced owner budgets, visible lag/gap policy and continued cancellation/fairness. Disable credit propagation or owner grouping to make it fail. |
| Many descendants each open many nearly idle or bursty flows. | Opening more identities/FDs cannot multiply the owner's aggregate allocation; admission remains bounded. |
| Force short reads/writes, `EINTR`, `WouldBlock`, wrapped queue slices, allocation refusal, EOF and reset at each boundary. | Exact byte order, retained tails, independent closure states and released credits. Remove cursor advancement/tail retention or an unblock reason to make it fail. |
| Guest send-side close followed by delayed response, and the reverse direction. | Accepted bytes drain; response survives; no implicit full-close. |
| End-to-end TLS server with ordinary trust validation, partial records and orderly/abrupt closure. | Identical payload and intact certificate behavior; relay injects no trust anchor or plaintext rewrite. An opaque relay reports transport closure without pretending to validate encrypted `close_notify`. |
| UDP empty, maximum negotiated size, undersized receive, reorder, duplicate, EAGAIN and full queues. | Whole datagrams or explicit recorded loss/error; never stream concatenation or fabricated delivery acknowledgement. |
| Internal domain/member/IPv6/loopback/Unix/VSOCK route cases. | Every permitted edge hits its assigned release counter; denied edges reach no receiver. Remove one forced-route rule and the bypass test fails. |
| Malformed lengths, overflow, reserved kinds, impersonated labels, forged control-like JSON, cross-run resume and observer authentication. | Refusal before unsafe allocation/authority use; bytes cannot become supervisor commands. |
| Multiplexed flow blocks, flood while cancel/revoke, FD reuse after closure. | Other admitted work and control continue; stale events cannot act on a new descriptor owner. |
| Durable spool fills, permission fails, storage disappears and broker restarts mid-record. | Quota bound, explicit failure, correct retained offsets/gaps, no unrelated data cleanup and no claimed exactly-once external operation. |

For benchmarks, compare the same integration SHA with existing routing/capture and the proposed relay on real guests for KVM, HVF and WHP, both guest architectures and all supported host targets where available. Record compiler/kernel/pin/OS/host resources, active flows, actual budgets, link bandwidth/RTT/loss, CPU scheduling contention, and each policy mode. Include interactive small output, bulk transfer, stderr-heavy mixed output, many owners, one slow reader, mixed UDP/TCP, encrypted streaming, reconnect and memory/disk pressure.

Measure first-byte and chunk latency, throughput, CPU time, syscalls, actual copies, allocated and resident memory including retained buffers, kernel socket/pipe memory, spool/page-cache use, fairness/starvation, cancellation latency, dropped/gap counts, and recovery. Report sample count, p50/p90/p99/max and uncertainty/noise. Follow observable progress under contention; a loaded machine's missed guessed deadline is not evidence of a broken relay. Mutation checks must show that the security/backpressure assertion actually detects its missing mechanism. Measurements validate the derivation; they do not turn this machine's numbers into production constants.

## 11. Open decisions and implementation order

1. Agree on canonical runtime-only versus verified per-process/tool attribution levels, and on which tool adapters provide semantic interception. Keep unknown visibility explicit.
2. Specify output endpoint direction, authenticated subscriber rights, wire framing, and `auto` versus an explicitly chosen default port. Keep ingress publication separate from outbound model-service access.
3. Specify required output/audit delivery, bounded spool authorization, observer gaps, disconnect, cancellation and quota failure behavior. No hidden lossless/durable promise.
4. Audit every route and descriptor edge. Add forced mediation for internal/alternative paths before claiming coverage for every controlled port.
5. Implement native private capture plus typed source/channel records, relay lifecycle/credit accounting, aggregate admission and owner fairness. Reuse existing Shards queue/poll/control seams.
6. Run the focused adversarial/mutation cases, then real-VM functionality and measured throughput/latency/memory gates. Optimize copies or batching only after the same invariants remain testable.

No benchmark was run for this research. Minimal overhead, complete port coverage, correct deep process attribution, durable replay, and performance parity remain implementation and verification obligations—not conclusions from the cited proxy designs.
