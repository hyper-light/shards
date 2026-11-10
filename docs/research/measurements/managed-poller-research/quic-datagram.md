# UDP datagram storage and managed QUIC credit

POLICY_READ implementer `01a1c5bda0bf766683384c0705c0a9bc`; coordinator
`01a148884a4770fdaf0111f3f792a248`; full canonical/checkout policy, plan and
handoff loaded. Rules SHA-256:
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
This is the next approved literal/caller research item, not an audit, selected
architecture, production fix or measured performance result. Production is
unchanged. Root owns all runtime reproduction, measurements, audits and merges.

Source base is `24f48ee5ebf198485117aa93bf47685ad7d7d53f`; eight focused local
source/dependency blobs match main
`3ea2164dd98878426936c880f0e8d542ef67d98f`. The dependency remains hyper-raft
`233b7fcef7a363327c5439b191724a38065062cd`. The
[identity record](quic-datagram-source-identity.json) retains sixteen external
byte identities, connector Git blob matches, exact URLs and retrieval times.
Primary-source versions are research pins, not latest-release or current-host
claims. No runtime number or generic transport/allocator abstraction is proposed.

The [exact Kiln research export](quic-datagram-kiln-research.md) retains grades
and numbered primary citations. These QUIC inferences are B/79 and B/80, with
all seven cited code quotes found. Automated grades are not runtime evidence,
semantic certification or a selection criterion.

## Resolve the word datagram first

`managed::DATAGRAM_BYTES` aliases `net::DATAGRAM = 65_527`. It reaches the
Driver's UDP receive scratch Vec, outbound UDP datagram allowance and receive
turn calculation. **It is not an RFC 9221 DATAGRAM frame budget.** This managed
transport explicitly disables incoming application DATAGRAMs and sets their
send buffer to zero. Application output/control travels on a reliable
bidirectional stream. [Literal](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L22),
[alias](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L85),
[transport](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L516).

| Quantity | Protocol/implementation meaning, distinct from chosen storage |
| --- | --- |
| 65527 | RFC 9000 §18.2's default `max_udp_payload_size`, stated as maximum permitted UDP payload; values below 1200 are invalid. This is the advertised willingness to receive, separate from the path. It does not select queue bytes, batch count or an application's record size. [RFC 9000 §18.2](https://www.rfc-editor.org/rfc/rfc9000.html#section-18.2). |
| Initial 1200-byte datagram requirement | Specified Initial-containing datagrams need padding/coalescing to meet the minimum; it is not a universal allocation or desired MTU. [RFC 9000 §14.1](https://www.rfc-editor.org/rfc/rfc9000.html#section-14.1). |
| Current advertised receive maximum | The pinned `EndpointConfig::new` uses `1500 − 28 = 1472`, and its setter accepts 1200 through 65527. The Driver uses `EndpointConfig::default`; transport-parameter construction copies that field. Thus the protocol's absent-parameter default and this implementation's advertised default differ. Neither is selected here. [Endpoint config](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/config/mod.rs#L57), [parameter construction](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/transport_parameters.rs#L157). |
| Send datagram size | Peer receive willingness, the current path and QUIC packet/framing/protection constraints limit transmission. Local receive advertisement alone does not bound outgoing items. The pinned MTU mechanism applies peer maximum and path-discovery state. [Pinned MTU discovery](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/mtud.rs#L75). |
| Stream/connection flow credit | Reliable STREAM byte-offset limits have both stream and connection scopes. Resetting one direction does not terminate the reverse direction. These limits are separate from UDP boundaries and local memory admission. [RFC 9000 §§4.1–4.4](https://www.rfc-editor.org/rfc/rfc9000.html#section-4.1). |
| Congestion control | RFC 9002 describes per-path sender congestion control in bytes and permits other conforming algorithms. ACK-only packets differ from congestion-controlled in-flight traffic. Its example algorithm/defaults do not select application buffers or authoritative entitlement. [RFC 9002 §7](https://www.rfc-editor.org/rfc/rfc9002.html#section-7). |

The maximum conventional UDP payload is not a guarantee that a particular
address family, path or socket will send that many bytes. A larger receive
scratch buffer also does not force the peer or congestion controller to send a
larger datagram. QUIC packet coalescing, UDP datagram boundaries, STREAM frame
lengths and application record boundaries must remain distinct in any proposal.

## Actual storage, service turns and failure behavior

These are focused source observations, not measured allocation or fairness
results. The [named Driver source](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs)
provides the cited functions:

* `bind_bounded` requires positive connection/command allowances and at least D
  queued datagram bytes, where D is the protocol-max literal. Those limits do
  not reserve kernel/host memory; the public documentation requires a held
  caller reservation. [Admission](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L548).
* `Driver::run` fallibly provisions one reusable receive Vec for D bytes before
  entering the loop. It does **not** make a new scratch allocation per packet.
  A received prefix is copied into `BytesMut` before endpoint processing; its
  retained backing can then outlive the scratch read. This copy and downstream
  retention have different costs from scratch capacity.
  [Receive loop](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L638).
* After one blocking receive, the bounded nonblocking drain uses
  `queued_datagram_bytes / D` as its turn limit. Managed admission passes D, so
  the computed allowance is one datagram per such receive turn. This couples a
  service decision to a storage allowance; it is not a protocol fairness bound.
  Commands/timers are processed on outer turns. No control-latency bound follows
  from that source ordering.
  [Turn calculation](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L688).
* `queue_datagram` sums existing payload Vec **capacities** with checked
  arithmetic, checks incoming length, fallibly reserves exact payload storage,
  checks the resulting capacity, then fallibly reserves one queue entry. This
  guards payload retention; the payload sum does not itself count deque slots,
  the temporary transmit buffer, connection backing, socket queues or allocator
  overhead. Post-allocation capacity refusal differs from a reservation before
  every induced allocation. No excess retention or refusal bug is reproduced.
  [Queue](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L1008).
* `transmit` first tries retained output; when it remains blocked it yields.
  Otherwise it obtains data from QUIC with `poll_transmit(now, 1, buffer)` and
  queues/copies the reported prefix. `flush_datagrams` keeps a datagram on
  WouldBlock and removes it on success or another send error, leaving transport
  recovery separate. UDP send success is not application consumption.
  [Transmit/flush](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L1041).
* Owner revocation has a separate stop flag and wake datagram. The outer loop
  checks it, clears bounded queued output and closes connection state. A full
  data handoff therefore is not the only cancellation signal in source. Wake
  failure, a blocked syscall, temporary copies and long processing/transmit work
  still need actual deadline/control/cleanup witnesses.
  [Stop request](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L95),
  [command processing](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L745).

### A smaller scratch buffer needs a truncation contract

The advertised default is not proof that all larger UDP datagrams are rejected
before allocation. The pinned endpoint handler receives an already owned
`BytesMut` and decodes it. This focused read supplies no universal discard-before-
retention guarantee for a peer exceeding its advertisement.
[Pinned handler](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/endpoint.rs#L322).

Rust's `recv_from` contract permits excess bytes to be discarded and explicitly
defers platform differences. Linux documents message truncation and
`recvmsg`'s truncation flag. Winsock documents `WSAEMSGSIZE`, a copied prefix and
discarded UDP excess; zero-length UDP is a datagram, not stream EOF. A shortened
ordinary `recv_from` buffer cannot be assumed to distinguish a complete
buffer-sized datagram from a truncated one on every supported target. No new
platform adapter or peeking loop is selected here.
[Rust API snapshot](https://doc.rust-lang.org/std/net/struct.UdpSocket.html#method.recv_from),
[Linux API](https://man7.org/linux/man-pages/man2/recv.2.html),
[Winsock API](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-recvfrom).
The Rust page identifies std 1.99.1; this is API-source identity, not an observed
compiler. Cargo's rust-version 1.98.1 is an MSRV, not a pinned/queried toolchain.

## Three different credits and their owners

| Credit/store | Existing source contract and implication |
| --- | --- |
| QUIC receive limits | Server `transport` uses CONTROL_BYTES for both connection and stream receive windows. Client configuration uses `record_bytes + DATA_HEAD`; arithmetic/conversion is checked. The helper sets one peer bidirectional stream and no unidirectional streams. These byte windows are not UDP packet allowances. [Configuration](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L516). |
| QUIC send storage | `send_window` receives admitted transport bytes. Pinned `write_limit` takes the minimum of remaining peer connection credit and remaining local `buffered_data` allowance; stream writes apply the stream limit too. ACK handling releases only the storage actually freed, including selective-ACK retention. The configuration warns that a slice can retain larger backing allocations. This bounds a logical stored-byte measure, not every connection allocation. [Transport docs](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/config/transport.rs#L133), [write limit](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/state.rs#L988), [ACK release](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/state.rs#L846). |
| One retained application record | `Producer::submit` accepts an owned nonempty Vec only from Ready/Open and checks **capacity** against the admitted record allowance plus checked end offset. It returns ownership on typed rejection. `Service::flush` advances partial writes and retains the record as AwaitingAck; `consumed` requires its exact application end offset before Ready/Finished. QUIC transport ACK alone does not release this application record. [Producer](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L328), [flush/consumed](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L771). |
| Guest/daemon credit and reservation | `send_credits` grants the channel's current allowance and later submits that channel's captured record. `Output::capacity` can raise the learned record allowance only after `Charge::grow` succeeds, charging the overlapping VM/daemon record copies. `Producer::admit` explicitly does not enlarge the fixed QUIC send window. Initial record capacity derives from actual encoded active-label Open/terminal framing. These are separate authority/retention contracts, not interchangeable credit counters. [Capacity](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L192), [grant/submit](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L856), [cold framing](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L1058). |

Disconnect returns the service to Listening and preserves/restarts an unconsumed
record for a matching subscription cursor. Normal finish waits for application
acknowledgment; abort records accepted/consumed offsets, wakes waiters and requests
Driver stop. These source transitions do not prove current EOF, metadata,
reconnection, deadline or resource-return behavior. Root's separate actual
normal-exit investigation must remain authoritative; no guessed terminal fix
follows from this note.
[Disconnect](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L662),
[finish/abort](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L453).

## Alternatives and data-derived methods, all unselected

| Alternative | Derivation and cold state | Necessary proof, tradeoff and failure behavior |
| --- | --- | --- |
| Keep protocol-max scratch and existing bounded queue | Treat D as a compatibility envelope, separately account retained actual capacities, transient copies, queue entries and induced native/kernel/parent cost. Keep service-turn policy distinct conceptually. | Preserves current receive behavior; reserved/actual memory and throughput/control cost need live data. A logical payload bound alone does not establish hard aggregate admission. Cold allocation refusal, queue growth and partial setup must return owned resources. |
| Derive receive storage from explicit endpoint willingness | Evaluate one authoritative configured receive limit, chosen inside an admitted budget and communicated to peers, with complete oversize detection/discard semantics. Do not substitute the historical 1472 default or a page size. | Requires target-specific syscall behavior/ABI and a proof that a truncated prefix is never treated as a complete accepted unit. Send storage still derives from peer/path output, independently. New adapters only after existing shared-seam alternatives and actual need are established. |
| Demand-sized owned UDP storage | Evaluate a fallible per-datagram ownership/receive mechanism only if it preserves atomic boundaries, detects oversize and admits before retaining bytes. Compare reusable storage/copying with retained shared packet backing. | Peeking introduces races/extra syscalls if multiple consumers exist; repeated probes are not justified without evidence. Exact owner, syscall/dequeue ordering, failure cleanup and kernel charge matter. No buffer pool/cache or generic receiver is proposed. |
| Authority-bounded window/record learning | Use observed service rate and the full feedback delay to test a window method within held budget; separately model transport ACK/storage return and application-consumed feedback. Cold capacity must cover required encoded records/control, not a copied library default. | RFC 9000 §4.3 relates available flow credit to bandwidth-delay product, but that alone is no memory/latency guarantee. Reordering, application scheduling, record acknowledgment and metadata overhead alter actual demand. Growth is fallible before retention; learning never raises authority. [RFC 9000 §4.3](https://www.rfc-editor.org/rfc/rfc9000.html#section-4.3). |
| Independent service scheduling inside existing Driver | Evaluate receive/transmit work demand and control readiness separately from D/queue byte count, only after root measures a symptom or goal. | No arbitrary batch/time quantum is selected. A continuously hot sender cannot justify an endless drain; exact deadline/wake/neighbor progress and bounded work/CPU evidence are required. No duplicate connection/watch state or parallel scheduler is introduced. |

The pinned upstream Quinn endpoint and transport configuration implement the
same distinct receive advertisement, flow windows and send-storage concepts.
Their historical defaults are useful mechanism provenance, not a production
recommendation or comparative performance result.
[Upstream endpoint](https://github.com/quinn-rs/quinn/blob/eaec0db4bcb698f76df736d89938743c88a2ad5f/quinn-proto/src/config/mod.rs#L54),
[upstream transport](https://github.com/quinn-rs/quinn/blob/eaec0db4bcb698f76df736d89938743c88a2ad5f/quinn-proto/src/config/transport.rs).
No transport swap or crypto/trust/verification change is proposed.

## Coordinator execution requirements — every case unrun

Root must approve the exact existing seam/harness, source/locked dependency,
finite caller-supplied inputs, absolute deadlines and private resource envelope
before execution. This note has no approved executable candidate, so it does not
invent a command or present a source-derived hypothesis as a reproduced bug.

| Case | Independent inputs/witnesses and decisive result |
| --- | --- |
| Cold negotiated/absent receive parameter | Retain configured local willingness, actual peer parameter or absent default, path state and the actual UDP size. Separate valid transport acceptance from application record/cursor acceptance. Do not infer peer values from local defaults. |
| Whole/empty/oversized UDP and coalescing | On supported native hosts, use owned sockets to witness original datagram boundaries, native return/error/truncation, QUIC parse/accept/drop and resource ownership for zero, exact candidate boundary and over-boundary inputs. A zero UDP receive is not stream EOF. Test multiple QUIC packets per datagram separately from multiple datagrams. |
| Peer/path asymmetry and loss | Hold local receive config fixed while varying an explicitly supported peer/path bound. Witness actual emitted UDP lengths, MTU updates, loss/retransmission, byte-exact records and congestion behavior. Local receive size must not silently truncate legal outgoing results. |
| Queue capacity/refusal | Hold the socket unavailable under a bounded fixture, witness retained payload capacities, deque storage, transmit scratch and kernel queues separately. Exact fit/overflow, arithmetic refusal, allocator growth refusal and send errors must preserve the selected datagram/drop/transport-recovery contract and owned cleanup. |
| Distinct credit transitions | Stall application Consumed while transport ACKs continue, then separately stall transport delivery while the application remains willing. Record Ready→Record→AwaitingAck→Ready/Finished, peer flow limits, stored transport bytes, per-channel guest credit and held charges. Wrong/duplicate/stale end offsets and retained capacity exceeding payload length need independent rejection/ownership witnesses. |
| Learned record growth versus fixed transport | Record cold encoded-label/terminal size, observed quantum, admitted record capacity, reservation growth result and unchanged transport window. Growth refusal stays within current authority. A larger record must stream under existing transport credit or explicitly refuse; no larger entitlement is inferred. |
| Control and neighbor progress | One bounded hot data path plus an independent quiet channel/stop/deadline signal; witness actual control effect and original absolute deadline while receiving, transmitting, copying and draining. Include wake failure/full command handoff, peer disconnect and cancellation; cleanup precedes assertions. No timing allowance or starvation guarantee is invented. |
| Reconnect/finish/abort and resource return | Preserve an unconsumed record, reconnect with its authorized matching cursor, finish/abort independently and observe actual terminal/EOF/resource effects. Compare accepted/consumed evidence with root's normal-exit record; a source state alone is not a pass. Check parent/native/kernel/handle/queue return after owner ends. |
| Selected candidate's counterfactual and costs | Root removes only its actual size/credit/ownership guard, witnesses the corresponding real boundary/effect, restores exact source and runs a fresh closing case. Pair equivalent pinned workload/config/crypto with a credible baseline. Retain raw caller n, p50/p90/p99/max, CPU, allocations/reallocations/copies, retained/backing/native/kernel/parent bytes, faults and control/neighbor uncertainty. No 4–5× claim before those results. |

## R01–R16 and remaining gaps

| Rule | Evidence/limit for this checkpoint |
| --- | --- |
| R01 | No production edits. Fallible scratch/queue provisioning and infallible downstream copy/backing concerns are source observations only; refusal/panic-free proof unrun. |
| R02 | Exact specs, actual locked implementation and historical upstream mechanisms cited; alternatives conditional, root data/selection owed. |
| R03 | Protocol bounds, advertised defaults, wire-framing derivations, logical storage and learned demand distinguished; no replacement size/window/batch selected. Cold/overflow cases unrun. |
| R04 | Existing slot/subscription/finish/abort/credit transitions identified; actual cancellation/EOF/effect proof remains root work. |
| R05 | Existing Driver/Producer/Service/Charge seam only, no polling loop/cache/pool/generic abstraction added. Scheduling/receive alternatives require demonstrated need. |
| R06 | Focused literal/callers, concise independent contracts; no source complexity or refactor claim. |
| R07 | Sources and rationale explain limits rather than narrate a source change; production comments untouched. |
| R08 | No benchmark/4–5× result; credible paired baselines and misses owed. |
| R09 | No source suspicion treated as a live defect; terminal investigation remains with root and its dedicated lanes. |
| R10 | Durable raw-byte identities and bounded unrun method retained; before/after, cleanup and decisive mutations owed for a candidate. |
| R11 | Actual native UDP, seven-target and real-host behavior unrun. macOS arm64 only; Windows ARM VM remains paused. |
| R12 | Shared seams retained, no new platform adapter, host-root/admin/security/crypto change or deliverable. |
| R13 | Only selected datagram/window/credit scope and required ownership callers; no speculative architecture. |
| R14 | No profiling/measurement executed here. Induced retained/native/parent/kernel costs and faults are root obligations. |
| R15 | Original lane continues serial research/checkpoints; no explicit spawn/campaign/main merge. Prior automatic goal-session conflict reported and preserved. Coordinator review/execution/acceptance remains pending. |
| R16 | No local build/target/cache job/new full worktree/mirror/archive/cleanup; protected roots/evidence and paused VM untouched. Restored floor does not authorize heavy local builds. |
