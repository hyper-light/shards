# TLS Handshake envelope versus managed QUIC admission

This is the next coordinator-approved constants research item. It selects no
replacement size, transport architecture or production fix. Source observations
and derived proof obligations below are not reproduced memory failures. Root
alone executes the proposed cases, measures costs and selects implementation.

POLICY_READ implementer `01a1c5bda0bf766683384c0705c0a9bc`, coordinator
`01a148884a4770fdaf0111f3f792a248`: full canonical/checkout AGENTS, R01–R16,
development-evidence, CLAUDE, managed plan and current handoff read after resume.
Rules SHA-256 is
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Source base is `24f48ee5ebf198485117aa93bf47685ad7d7d53f`; the six named
Shards source/dependency blobs and five policy/plan files also match current
main `3ea2164dd98878426936c880f0e8d542ef67d98f` exactly. The
[identity record](tls-handshake-source-identity.json) gives exact URLs, connector
Git blob IDs independently matched against raw HTTPS bytes, SHA-256 hashes,
retrieval times and freshness limits. This is scoped reading, not a repository
audit. Production remains read-only; all files are inside the lane's owned
textual research directory. Existing
[Poller/inotify research](../../managed-poller-capacity.md) and its pending
runtime cases remain separate.

The [exact Kiln research export](tls-handshake-kiln-research.md) retains the
automated grades and source checks: these TLS inferences are B/80 and B/72.
One RFC 8446 citation check timed out in Kiln; the independent raw RFC retrieval
and hash succeeded in the identity record. Automated grades/checks are not
technical certification, live results or a selection criterion.

## What the literal establishes

[`daemon::managed`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L27)
defines `HANDSHAKE_BYTES = (1 << 24) - 1 + 4`, citing RFC 8446 §4.
The derivation is valid for the envelope of **one encoded Handshake message**:
one-byte type, three-byte unsigned body length, then that body. A handshake
flight can contain multiple messages; this envelope does not bound a flight,
an entire connection, simultaneous connections, retransmission storage or
allocator/kernel cost. [RFC 8446 §4](https://www.rfc-editor.org/rfc/rfc8446.html#section-4).

QUIC carries handshake bytes without TLS records; the TLS record-size limit
therefore does not supply this QUIC buffer bound. Ordered bytes awaiting TLS
processing and out-of-order/encryption-level data held by QUIC have distinct
owners. [RFC 9001 §§3–4.1.3](https://www.rfc-editor.org/rfc/rfc9001.html#section-4.1.3).

CRYPTO data has no stream flow control. RFC 9000 requires support for at least
4096 out-of-order bytes and permits different buffering across connection
phases. During the handshake, exhaustion requires expansion or connection
closure with `CRYPTO_BUFFER_EXCEEDED`; post-handshake handling has separate
discard/acknowledgment rules. These are protocol constraints, not a selected
product allocation, replenishment policy or aggregate budget.
[RFC 9000 §7.5](https://www.rfc-editor.org/rfc/rfc9000.html#section-7.5).

## Actual dependency and caller accounting

The authoritative dependency is **hyper-quic/hyper-tls at hyper-raft
`233b7fcef7a363327c5439b191724a38065062cd`**, selected by
[`crates/remote/Cargo.toml`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/Cargo.toml#L13)
and Cargo.lock. A registry rustls entry elsewhere in the lockfile does not
identify this path. Pinned vendoring records identify the historical upstream
mechanisms as quinn-proto 0.11.18 and rustls 0.23.45, from commits
`eaec0db4bcb698f76df736d89938743c88a2ad5f` and
`2976d90fd1c2db6b518700dd101b714069cfcb17`. Their archived-package hashes are
metadata in those records, not independently downloaded archives in this batch.
[Quinn provenance](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/vendor/quinn-proto/VENDORED.md),
[rustls provenance](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/vendor/rustls/VENDORED.md).

At the Shards source base:

* `start_managed_capture` computes `active_count = 2 × active.len()` with checked
  arithmetic, includes H once per active stdout/stderr channel alongside record,
  transport, control/datagram and structural terms, and calls `Charge::reserve`
  before constructing endpoints. Files-only member slots still have structural
  costs. Kernel buffer sizes are read and charged after endpoint construction.
  `Charge::grow` checks headroom and shared accounting; drop subtracts held bytes.
  These are logical admission operations, not an allocator interception or proof
  of complete induced-memory attribution.
  [Exact caller and charge](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L565).
* `transport` passes H to `crypto_buffer_size`. `Endpoint::bind` sets
  `max_incoming(1)` and both incoming byte limits to zero; its bounded Driver
  allows one connection. These are **per endpoint** settings. The managed run
  constructs one endpoint per active channel, so one endpoint's limit is not a
  whole-run concurrent-handshake limit. The API explicitly requires its caller
  to retain a reservation covering QUIC, endpoint and kernel state.
  [Admitted and transport](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L204),
  [Endpoint construction](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L551).
* The Driver calls the QUIC endpoint's packet handler before its
  `NewConnection` limit/refusal branch and then accepts a permitted connection.
  That ordering does not, by itself, establish the allocation cost of rejected
  packets or a quota bypass. Handshake expiration is based on the connection's
  original start plus admitted patience, and is separate from packet activity.
  Actual timer/control progress under load remains unrun here.
  [Receive](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L781),
  [expiration](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L959).

## Distinct retained stores in the pinned implementation

| Store or check | Exact observed semantics; remaining accounting obligation |
| --- | --- |
| QUIC CRYPTO receive reach | `read_crypto` selects the packet space, checks its frame end, and rejects `end − bytes_read > H`. It inserts into that space's assembler with an offset limit based on bytes already delivered. H is a reach/window check; it is not a direct check of all resident allocations. Ordered available chunks are delivered to TLS. [Pinned connection](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/mod.rs#L2910), [per-space assembler](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/spaces.rs#L53). |
| Fragment/packet retention | The assembler tracks buffered bytes and an allocation estimate, holds `Bytes` slices and heap metadata, and can compact fragmented storage with new backing storage. Its allocation estimate explicitly can count the same packet allocation multiple times. Logical unique bytes, metadata, actual shared backing and compaction peak are different quantities. Existing source heuristics are not adopted as product derivations. [Pinned assembler](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/assembler.rs#L162). |
| Undecryptable packet queue | `hold_undecryptable` separately compares its packet-byte counter plus candidate length with H during handshaking, subject to peer/key state. This is another use of H, not the same counter as the CRYPTO assembler. The source does not establish that every possible maximum occurs simultaneously; a measured reachable-state analysis is still required. [Pinned hold predicate](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/mod.rs#L3160). |
| Ordered TLS handshake storage | The adapter feeds bytes to hyper-tls `read_hs`; that function first extends its `DeframerVecBuffer`, then dissects/coalesces messages and processes packets. The message-body parser limit is separately `0xffff`; it excludes the four-byte header. It is not H and not an aggregate flight/connection limit. [QUIC adapter](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/crypto/rustls.rs#L103), [read_hs](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-tls/src/quic.rs#L383), [body parsing/limit](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-tls/src/msgs/deframer/handshake.rs#L267). |
| TLS capacity after consumption | QUIC's `extend` uses checked end arithmetic but grows the Vec with `resize`; `discard` updates used data without shrinking capacity. The socket-read path's separate `prepare_read` cap/shrink logic is not called by this QUIC append path. Consequently neither that socket cap nor consumed-byte count proves QUIC TLS retained capacity. This is a source observation, not a leak or allocation-refusal reproduction. [Pinned buffer](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-tls/src/msgs/deframer/buffers.rs#L174). |
| Outbound and remaining connection state | The connection separately obtains outgoing handshake data, while TLS owns outgoing handshake queues and optional flight storage. Their lifetime, retransmission retention, shared configuration and native crypto allocations require separate attribution. This focused read does not claim a complete allocation inventory. [write_crypto](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/mod.rs#L2986), [TLS QUIC state](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-tls/src/quic.rs#L466). |

Upstream Quinn's same setter describes out-of-order crypto buffering and has a
16 KiB default. Upstream rustls enforces the same 65535-byte message-body limit.
These independently pinned implementations establish mechanism/default
provenance; **neither value is a recommendation**. Copying either into H would
confuse a receive reach/window, a parser bound, a flight and aggregate admission.
[Upstream Quinn](https://github.com/quinn-rs/quinn/blob/eaec0db4bcb698f76df736d89938743c88a2ad5f/quinn-proto/src/config/transport.rs#L270),
[upstream rustls](https://github.com/rustls/rustls/blob/2976d90fd1c2db6b518700dd101b714069cfcb17/rustls/src/msgs/deframer/handshake.rs#L373).

## Alternatives to evaluate, with no selection

| Alternative | Derivation and cold-start behavior | Failure, compatibility and evidence needed |
| --- | --- | --- |
| Retain the protocol-envelope H while separating its accounting meaning | Keep existing transport behavior while derive separate reachable-state bounds for receive backing/metadata, ordered TLS, send/retransmit, endpoint, configuration and native/kernel costs. Sum per admitted connection/channel and aggregate run; measure transient overlap. | H alone still supplies no hard total-memory guarantee. Cold setup, reservation refusal, partial construction and teardown must be witnessed. A conservative reservation can refuse useful work; quantify that separately from actual allocation. |
| Owner-budget-derived receive reach, distinct from TLS message framing | Derive an available CRYPTO allowance from retained owner authority after structural/control/other stores and the actual admitted connection count. Respect protocol support and the application's required valid handshakes; use live data to validate the method. No numeric seed or learned headroom expansion is proposed. | Requires a demonstrated complete bound or fallible charge before every induced allocation. A smaller reach may reject valid reordering or flights even when each TLS message fits its parser. Preserve authentication, TLS versions and verification; an unsupported resource request is a typed refusal. |
| Accounted phase-sensitive allowance inside an existing budget | Evaluate initial admitted storage plus measured, authority-bounded growth/reclamation when protocol phase and actual retained demand justify it. This is a conditional method, not a proposed generic allocator or new manager. | Requires real API support and a proven reservation-before-allocation boundary, including compaction and native crypto. Growth refusal must close/refuse according to protocol while cancellation/control survive. Learned demand never grants extra entitlement; shrinking logical reach alone does not release retained memory. |
| Explicit handshake concurrency admission across existing endpoints | Use actual active endpoints/connections and held lifetime authority to account for simultaneous handshakes; evaluate admission scheduling only if root reproduces a need. | A scheduler would need an explicit lifecycle and fairness proof and must preserve required independent channels/control. Per-endpoint connection limits already exist; duplicate watched state or a parallel manager is not justified by this note. Cost and reconnect/partial-setup behavior remain unmeasured. |

All alternatives preserve the existing trust, raw-public-key authentication,
TLS 1.3 configuration and no-early-data behavior in the
[named TLS caller](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/tls.rs).
No security/crypto change or production allocation change is authorized by this
research. Before any candidate, root must supply the exact reproduced symptom
or measured goal and approve the smallest affected seam under R02/R09.

## Root-only reproduction and measurement proposal — all unrun

Use the exact Shards source, locked hyper-raft revision, actual toolchain,
supported host/kernel/architecture, and a bounded private peer using existing
identities/verification. Let the caller supply finite case counts, deadlines
and resource envelope; this note supplies no runtime number or automatic
sweep. Keep root's actual normal-exit/pre-send gates independent.

| Case | Required independent observations and decisive outcome |
| --- | --- |
| Cold start and no subscriber | Record trusted active member/channel map, computed per-channel/structural reservation, endpoint/connection counts and actual retained allocations before/after endpoint construction. Files-only holes do not become active endpoints. Failure/refusal releases every acquired endpoint, queue and charge. |
| Valid ordered and reordered handshakes | Retain actual phase/space, CRYPTO offset/end, bytes delivered to TLS, queue/backing identity, capacities and TLS parser progress without secrets/content. Fragmentation/reordering is an explicit input. Successful verification and managed subscription are separate witnesses from transport receipt. Record reachable overlaps; do not assume all configured maxima coexist. |
| Boundary and gap refusal | At a named method-derived candidate reach, exercise just-within/over boundary and sparse fragments using a controlled private transport fixture. Witness typed CRYPTO exhaustion versus TLS message-size rejection independently, including bytes buffered before rejection. Test transport's required minimum support separately from application handshake compatibility. No silent truncation, trust fallback or false successful subscription. |
| Ordered partial message versus QUIC out-of-order storage | Deliver a valid handshake in controlled pieces; observe TLS used length/capacity while QUIC advances its bytes-read offset, then separately hold later out-of-order bytes. This distinguishes window movement from total retained storage. Capture compaction/reassembly transients and shared packet backing without double-counting. |
| Simultaneous endpoints and refused connection | Use the actual two-channel active-member topology, plus an independently bounded extra connection attempt at one endpoint. Witness when limit/refusal occurs and any temporary/configuration/kernel cost. Account aggregate ownership and release after partial setup, rejected connection, timeout, peer loss and endpoint stop. |
| Allocation refusal and cancellation | Root chooses a bounded refusal mechanism at an exact allocation/charge seam, including first cold use and growth/compaction. Witness typed propagated outcome versus abort, original-deadline progress, actual connection teardown and reservation/resource return. Exercise cancellation during handshake independently of idle/handshake timers; packet activity must not be assumed to prove control progress. |
| Retention after handshake/close/reconnect | Record logical used data and actual capacities/ownership after successful handshake, application transfer, connection close and endpoint teardown. A retained reusable allocation is distinct from a resource still owned after its owner ends. Repeat only with caller-supplied finite count and independently establish handle/kernel/parent return. |
| Decisive guard checks and paired costs | For a selected change only, root independently removes/restores its actual resource/refusal boundary and witnesses actual excess retained ownership or the missing typed effect, then a restored pass. Compare equivalent pinned competitors/workload/configuration without crypto weakening. Report raw n, distributions, CPU, allocations/reallocations/copies, retained/native/kernel/parent cost, faults, control/neighbor progress and uncertainty; source sizes are not benchmark results. |

No exact command invoking a fixture is supplied because no approved executable
candidate/harness or root case envelope yet exists for this item. Root's next
implementation approval should name those inputs and the precise observation
seams first. These rows are actionable harness requirements, not claimed runs.
Platform checks must cover the relevant seven targets and actual supported
hosts; skipped or unavailable hardware stays unverified. Windows ARM remains
paused. macOS is arm64 only. No local target/cache/build job was created; root's
headroom receipt was 33,299,759,104 bytes, with local heavy builds/new full
worktrees still prohibited.

## R01–R16 account for this documentation checkpoint

| Rule | Applicable evidence and remaining gap |
| --- | --- |
| R01 | No production edits. Checked arithmetic versus infallible dependency growth is identified only at named stores; no allocator-refusal or panic-free proof. |
| R02 | Exact RFC/pinned dependency/upstream alternatives and owner/compatibility distinctions retained. Live selection data and coordinator review owed. |
| R03 | H's single-message derivation separated from protocol minimum, library defaults and aggregate admission. No replacement literal/method selected; cold/refusal/retention cases unrun. |
| R04 | Existing endpoint/connection/handshake/refusal/drop paths identified; proposed measurements require explicit phase/terminal witnesses. No new state machine. |
| R05 | Existing Admitted/Charge/Driver/TLS seam only. No generic abstraction, parallel ledger or duplicate cache introduced; concurrency alternative is conditional on reproduced need. |
| R06 | Focused call path and bounded proposal, no source refactor or complexity claim. |
| R07 | Documentation explains derivation and limits, with exact primary citations. No source comments changed. |
| R08 | No speed/resource superiority claim. Equivalent baselines, raw distributions and target misses remain root work. |
| R09 | Source-derived aggregate-accounting obligations are not reproduced defects. No fix before root's actual reproduction/measurement and approval. |
| R10 | Durable identity/freshness and unrun case matrix retained. Before/after, cleanup and mutation results owed for any selected candidate. |
| R11 | Seven-target/actual-host execution unrun. macOS arm64 only; paused Windows ARM untouched. |
| R12 | Existing shared transport/caller seams read; no new platform adapter, host privilege, crypto weakening or extra installed binary. |
| R13 | Research stays within the literal and necessary callers/stores. No production architecture added. |
| R14 | No measurement/profile/benchmark executed by implementer; root owes induced parent/native/kernel costs and uncertainty. |
| R15 | Root owns audit/runtime campaigns/combined validation/main merge. A signed named-path doc return is not managed-plan or lane completion. Automatic planning-step sessions were reported to root for serial ownership correction, with existing work preserved. |
| R16 | No local build/cache cleanup/new full worktree/archive. Root targets, evidence and paused VM preserved; no active local target/cache/job. |
