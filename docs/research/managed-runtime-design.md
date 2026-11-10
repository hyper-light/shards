# Managed agents and harnesses: extend Shards' existing boundaries

Research date: 2026-11-09 Pacific / 2026-11-10 UTC. The supporting reviews inspected
`ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8`. The coordinator checked the controls below
and confirmed that their implementation files are unchanged through integration
`ccf718164f0a107fc240536447fbb2c50128ee7f`, now included in main
`e9f7d6bc1f27a1a42b42a5bfbc10fbad8d9960bf`. This is a research-backed design proposal;
the proposed management layer and its acceptance tests are not implemented here.

## Recommendation

Make Shards' existing domain the unit of ownership, authority and aggregate cost.
Extend its native launcher, private tool server, process history and network stack
into one checked management path. An AGENT/HARNESS artifact, its manifest and every
descendant remain untrusted, including a normally cooperative Codex or Claude CLI.
The boundary must still work when the harness disables hooks, lies, forks, redirects
output, saturates a queue or deliberately attacks its parent.

The differentiating capability is the connection between **permission, the actual
operation, its owned resources, and its recorded outcome**. Shards already owns
several of those seams. The new layer should join them while retaining the existing
kernel restrictions; a stream of security alerts alone cannot establish that chain.

Four supporting reviews contain the primary sources, source observations and
adversarial cases behind this proposal:

- [Process authority, Linux mechanisms, Falco/Sysdig and Tetragon](managed-runtime-process-control.md).
- [Buffer ownership, internal/external routing, output and network relay](managed-runtime-buffered-relays.md).
- [Adversarial model, resource attacks, Falcon, Chromium, gVisor and capabilities](managed-runtime-adversarial-model.md).
- [Native Codex/Claude interfaces, tool brokerage, credentials and rich results](managed-runtime-tool-broker.md).

Facts about current code, upstream contracts, design inferences and unverified
properties are distinguished in those reviews. Product descriptions are evidence of
the vendor's stated design, not independent proof of effectiveness or overhead.

## Existing controls and what the new layer adds

This ledger is a source review, not certification that every composition has passed
an adversarial test. Keep these controls in place when adding a new data path.

| Existing control | What the inspected implementation establishes | Required extension or evidence gap |
| --- | --- | --- |
| [Build and runtime Agentfile checks](../../crates/shards/src/agentfile.rs), D109 | Derives grants from checked image content rather than trusting image labels. Rechecks unsafe runtime overrides beside domains. | The small local manifest must normalize into the same plan. It cannot manufacture grants, endpoints or privileged launch options. |
| [Native domain launch](../../crates/init/src/domains.rs), D59 | Creates the child directly in its cgroup and private mount/PID/IPC/network/UTS/cgroup namespaces before exec. | Retain a stable leader handle and an owned domain lifecycle; register output and resource ownership before the first untrusted instruction. |
| Same launcher: IDs, capabilities, descriptors | Distinct UID/GID; no supplementary groups or capabilities; `no_new_privs`; closes descriptors above stdio. | Keep management handles and supervisor secrets private. Review every newly inherited/received FD as an authority transfer. Parentage alone is not protection. |
| Same launcher: filesystem and IPC view | As-built read-only system, private scratch and IPC, hidden sibling directories, restricted device nodes and explicit volumes. | Account for writable grants, executable/JIT files, scratch, shared memory and inode/dirty-page pressure. A granted write remains a permitted effect unless a narrower broker owns it. |
| [Domain syscall profile](../../crates/shards/src/setup.rs), D59 | No-capability seccomp profile; socket-family restrictions; excludes interfaces such as io_uring, BPF/perf, keyrings and userfaultfd as described in that source. Landlock is applied by the launcher. | Preserve the actual profile and ABI coverage; do not add broad syscall permissions just to make an interceptor work. Test alternate operation paths. |
| [Sibling workload checks](../../crates/shards/src/agentfile.rs), D115 | Rejects privileged/host-namespace and crossing-capability configurations that would expose domains; checks reserved identities and relevant network overrides. | A hostile primary workload belongs in the same threat model. New endpoint placement must not grant it domain output or control access. |
| [Domain cgroups and memory governor](../../crates/init/src/domains.rs) | Enables domain memory/PID controllers, optional individual asks, aggregate memory protection and whole-domain termination. | Optional asks do not establish mandatory individual CPU/IO/FD/storage fairness. Producer-induced supervisor allocations also need logical charges. |
| [Private per-domain server](../../crates/server/src/server.rs), D60 | Private socket, per-instance mutual TLS, separate identity and confinement; messaging/file transfer checks grants before effects. | Extend this broker rather than granting a harness the supervisor's authority. A readable client key identifies its domain, not a trustworthy executable. |
| [Code-mode workers](../../crates/server/src/sandbox.rs), D136 | Restricted worker executes code and requests allowed tools from the instance; does not inherit instance keys. | Reuse the owned request/worker pattern for compatible native tool execution. External MCP discovery currently returns endpoints/commands; it is not a universal tool proxy. |
| [Scoped paths](../../crates/init/src/inroot.rs) and [host volume grants](../../crates/shards/src/volumes.rs) | Holds directory/object authority and resolves paths in the intended root; host paths remain owner-granted objects. | New file tools must act on checked objects, with rename/symlink/magic-link and non-UTF-8 race tests, rather than check a path then reopen it globally. |
| [Process recorder](../../crates/init/src/procevents.rs), D123 | Kernel fork/exec/exit history with explicit loss/unavailable state and argv provenance; excludes threads. It explicitly says it is a record, not a boundary. | Preserve that honest scope. Thread/resource membership and complete pre-action audit require separate mechanisms; a missing event never authorizes an effect. |
| [Domain network plan](../../crates/init/src/netplan.rs) and [Switch/Gate](../../crates/init/src/links.rs), D61 | Explicit directed grants, network membership and kernel routing/filtering between namespaces. | Kernel-routed internal traffic does not yet satisfy a universal userspace buffered-relay contract. Include it explicitly in the new coverage plan. |
| [Host network stack](../../crates/net/src/lib.rs), [TCP engine](../../crates/net/src/tcp.rs) | Owns external TCP flow state and retained unsent bytes; validates applicable addresses/grants. | Add aggregate credits and release/audit policy. Inventory peer forwarding, UDP, IPv6, DNS, ingress and bypass routes instead of assuming all reach the external TCP gate. |
| [Current domain output](../../crates/init/src/domains.rs), [relay](../../crates/init/src/run.rs), [framing](../../crates/init/src/frames.rs) | Automatic capture exists, but stdout/stderr share a pipe and output is rewritten into prefixed lines on the main workload's stderr. | Separate channels, preserve raw bytes and move default delivery to an authenticated private endpoint. Avoid changing the primary workload's protocol stream. |
| [Host VM confinement](../../crates/shards/src/confine.rs), [grants](../../crates/shards/src/grant.rs), D30/D141 | Existing Linux Landlock/seccomp, macOS App Sandbox and Windows AppContainer/granted-handle architecture restrict the VM process itself. | Preserve this outer boundary. New guest management should not require host root, a host sensor driver, or another installed binary. Host checks do not prove the new guest controls passed on every backend. |

The source review also found an explicit policy question: the aggregate memory
governor selects the largest populated domain under pressure. A smaller attacker
could potentially provoke pressure while a larger legitimate sibling becomes the
victim. That is an **inference to test**, not a reproduced exploit. The existing
owner-chosen aggregate policy cannot silently be called per-sibling isolation.
Stronger entitlement protection requires an explicit admission/accounting policy;
an estimator cannot decide whose grant should be sacrificed.

## Separate authority, execution and evidence

Use a small internal typed interface around deep implementations, without requiring
more fields in the user's two-field startup manifest. Proposed responsibilities:

| Owned object | Responsibility |
| --- | --- |
| `ManagedDomain` | Immutable run/domain generation, launch plan, kernel handles, confinement, admitted budgets, whole-domain stop and reaping. |
| `ToolCall` | Authenticated caller, concrete capability, validated arguments, resource reservation, owned execution, cancellation and exactly one terminal result. |
| `ManagedStream` | Domain/channel identity, byte offsets, retained credit, independent half-close/drain/cancel state and authorized subscriber cursors. |
| `EvidenceRecord` | Trusted provenance, policy revision, operation/outcome and explicit loss/uncertainty; untrusted payload/correlations remain separate. |

These are design names, not a committed Rust API or wire schema. Ordinary OS policy
should decide ordinary forbidden operations without a userspace round trip per
syscall. A broker performs security-sensitive operations on copied, checked data
under its own narrow authority. Inspecting mutable target memory and then letting
the target syscall continue is insufficient; see the
[seccomp notification TOCTOU contract](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html).

Provenance needs separate variants for broker-enforced facts, kernel observations,
adapter reports and unattributed domain bytes. A harness-provided tool ID, PID,
trace header, environment variable or log prefix cannot select its principal or
increase its rights. PID/start identity and retained kernel handles avoid bare-PID
reuse. Cgroup membership, not walking a potentially raced PPID tree, contains
descendants through exec, double-fork and session changes.

The tool lifecycle reserves authority and result resources before an effect;
termination first closes admission, then ends the complete domain and reconciles
its processes, handles and streams. Cancellation is not rollback of a remote
request or file write already committed. Restore and move need a new runtime
generation and checked re-establishment of broker/observer authority before actors
resume; an old subscriber or request must not acquire a new domain with the same name.

Native functions and logical subagents can exist entirely inside one process. An
OS sensor cannot universally infer their tool names or intent. Exact semantic
authorization requires that the trusted broker own the effect and deny alternative
routes. Other permitted native activity retains OS/object restrictions with honest
coverage labels. This distinction also applies when users select Codex/Claude's
dangerous approval flags: Shards' boundary cannot depend on harness cooperation.

## Automatic capture and mediation of every reachable route

Install private stdout and stderr capture before exec. Descendants inherit those
channels automatically; no shell wrapper, manual redirection or user-installed
supervisor is needed. A shared inherited pipe authenticates domain/channel, not
the exact writer. Exact per-tool output comes from broker-created dedicated
capture or an owned tool protocol, never guessing the writer from payload text.

The all-ports requirement needs an explicit route ledger:

| Route | Required design decision or control |
| --- | --- |
| Internet ingress/egress, IPv4/IPv6, TCP/UDP/QUIC and DNS | Own the permitted flow and charge buffered bytes before release. Preserve protocol semantics; opaque encrypted payloads remain opaque. |
| Inter-domain and engine/peer traffic | Force admitted traffic through a Shards-owned relay; existing namespace routing/filtering remains an independent grant boundary. |
| Same-domain loopback, including a process connecting to its own listener | A router between namespaces cannot see this path. Evaluate a guest-local mediation hook or broker-only socket capability; prove coverage before claiming all ports. |
| Unix pathname/abstract sockets and passed socket FDs | Check namespace/object/peer grants. Do not pass the underlying unrestricted connection when it would bypass required mediation. A broker-fronted local endpoint may retain mediation. |
| VSOCK and management channels | Keep control inaccessible to actors; current domain socket-family restrictions are part of this boundary. A new bridge requires an explicit grant and ownership review. |
| Files, shared memory, memfds and other non-port output | Enforce concrete object grants and account resources. Port capture does not imply every permitted memory/file write becomes a semantic tool result. |

For native socket compatibility, compare guest-side forced relay and kernel-object
gating against broker-provided endpoints. This note does not select an unproven
transparent interception mechanism or assume a proxy environment variable enforces
anything. Current kernel configuration is not evidence that Falco/Tetragon tracing
prerequisites are enabled; inspect the exact effective kernel before such a change.

Buffering means owned, accounted streaming with admission and release rules. It
does not require collecting an entire response or adding a redundant copy to a
safe existing fast path. Preserve byte order, short writes, interruption,
half-close and EOF. Preserve datagram boundaries and report actual UDP failure or
loss. A transport ACK is not an application commit; reconnecting an arbitrary byte
stream does not authorize replay of non-idempotent work.

Every queue, flow, request, observer cursor, decompressor and artifact consumes an
admitted budget, including allocations made by the broker on a child's behalf.
Hierarchical scheduling charges the owner/subtree first, so creating more sockets
or descendants cannot multiply entitlement. Backpressure, quota-charged spool or
typed refusal must resolve exhaustion; finite storage cannot retain infinite
output for a permanently stopped consumer. Optional observers may receive explicit
gaps. An operation requiring retained audit evidence must secure that capacity
before its effect. Lifecycle/control capacity must remain available under saturation.

Learn throughput, RTT, service cost, scheduling noise and useful batching inside
the admitted envelope. Read descriptor/memory/protocol bounds from their actual
sources. Learning never expands network grants, filesystem rights or resource
entitlement, and another machine's measurement never becomes a production constant.

## Safe evidence, display and credentials

Keep canonical bytes distinct from their display. Application bytes never become
control frames merely because they look like JSON-RPC or a Shards event. The
supervisor owns envelope identity, lengths, ordering and state transitions. Validate
framing and remaining credit before allocation; malformed fields must not crash a
kernel hook or global supervisor. Rich tool results retain structured values,
images/audio, files, links and error state. Following a resource link is a new
authorized operation, not permission inherited from a tool's result annotation.

Sanitize **the display/export appropriate to its context**, not arbitrary original
payloads: escape control characters and misleading terminal sequences, distinguish
untrusted labels, handle invalid UTF-8, avoid unsafe HTML/URLs, and make bidirectional
text effects visible where they could spoof identity. Raw terminal attachment is a
separate deliberate interface. Preserve raw byte retrieval only under its own
content access/retention grant. No sanitizer can guarantee discovery of every secret
encoded in arbitrary output. [OWASP log handling](https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html)
supports contextual encoding, secret minimization and resource-exhaustion testing;
[CWE-117](https://cwe.mitre.org/data/definitions/117.html) describes log-forging risk.

Keep trustworthy capture time separate from an actor's claimed event time. Export
versioned telemetry with bounded cardinality and queues, without making the export
format the authorization model. The [stable OTel log model](https://opentelemetry.io/docs/specs/otel/logs/data-model/)
distinguishes occurrence and observation timestamps. GenAI tool conventions remain
development interfaces; the supporting tool review pins their inspected revision.
Trace IDs and tool arguments are not grants. Ordinary telemetry should minimize
content; raw bodies, environments and auth material must not appear by default.

A hostile process that can read `auth.json` has received those credentials. A
read-only mount or output masking does not change that. Retaining the user's native
signed-in CLI and keeping the credential opaque are different contracts; an opaque
inference broker requires a documented compatible client/provider seam. Never
weaken TLS or silently intercept encrypted provider traffic to manufacture semantic
visibility. The [tool/credential review](managed-runtime-tool-broker.md) records the
native options and compatibility work still required. No credentials were read or
copied and no provider requests were made for this research.

## Endpoint and manifest decisions

Keep the user's simple root `agent.json` / `harness.json`: required `entrypoint`,
optional literal `options`, checked command words and no implicit shell evaluation.
HARNESS/AGENT supplies the artifact; NETWORK and EXPOSE retain explicit grant
semantics. No installation through RUN or VOLUME is required by this design.

Capture should always be established automatically, regardless of whether an
observer is connected. Default delivery should be a private authenticated endpoint,
separate from the main workload's stdout/stderr. A proposed default is an
OS-assigned service endpoint excluding 80/443, reported to the owner; no fixed port
has been selected. Listener versus push direction, endpoint wire protocol and
authentication/resume contract still need a concrete API decision. An internal
output endpoint must not automatically create public ingress. Output ports,
egress-proxy ports, VSOCK service ports and remote HTTPS destination ports have
different roles; reusing a number does not itself create a protocol conflict.

## Implementation and proof obligations

| Slice | Concrete result required before claiming completion |
| --- | --- |
| Local startup manifest | Native and BuildKit frontend agree; real directory/archive launches prove the manifest controls the intended child; malformed/symlinked descriptors refuse correctly. The isolated implementation branch is separate from this research. |
| Owned domain lifecycle | Kernel handle captured at birth, full population stop/reap, explicit startup failure and owner death, no management FD inheritance; tests include fork/thread storms, PID churn, exec and daemonization. |
| Private managed streams | Separate exact-byte stdout/stderr, versioned framing and authenticated subscriptions, no primary-protocol corruption, bounded retention, cancellation progress under a stalled consumer. |
| Resource and network mediation | Route ledger covers loopback/internal/external and alternate APIs; every allowed stream owns credits; hostile sibling/fanout/FD/storage pressure stays inside the admitted policy. |
| Typed tool execution | Native adapter method/fallback inventory, broker ownership before effect, monotone delegation, rich results and cancellation/replay races; malicious binaries cannot bypass object policy by ignoring adapters. |
| Durable evidence and telemetry | Provenance/loss state is explicit, actor data cannot forge records, secret/content access is separate, failures cannot remove enforcement or deadlock control. |
| Portability and cost | All eight host targets compile/lint, real guest E2E on supported KVM/HVF/WHP combinations, paired measurements of launch/tool/relay/cancel latency, CPU, allocations/copies, resident/retained memory, handles and neighbor impact. |

Mutation-check each enforcement test by removing the particular guard and observing
the forbidden effect, rather than merely losing an alert. Fuzz frame parsers and
state machines; inject ENOMEM/EMFILE/ENOSPC, partial I/O, collector/broker death,
corrupt/stale policy and restore/move failures. Measure both Linux guest architectures
with actual kernels, artifacts and host contention recorded; report n, p50, p90,
p99 and max. There are no measured performance claims for this proposed layer yet.

The guest kernel, trusted init, narrow broker and outer VMM remain trusted components.
This design does not establish protection against every kernel vulnerability,
microarchitectural covert channel, or abuse of deliberately delegated data access.
The guarantee must name actual permitted objects, effects and evidence coverage.

## Evidence record

Kiln research record: `01a1c33a30dd72afa53e3cbda62339cb`. Its checked citations and
automated grades are recorded separately in [the evidence ledger](managed-runtime-evidence.md).
Those grades assess supplied citation support; they are not a security assurance
rating or a substitute for the unrun implementation tests above.
