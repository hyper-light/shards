# Managed output terminal publication: conditional proposal

**Proposal only; implementation requires the coordinator's actual reproduction
and authorization.** The source observation below is not a reproduced bug.
Root's pending fixture at `fce89fb1063b5c2344f920ff9e967c023ce3abfc` requires
member 1's canonical End, application ACK and QUIC EOF on both channels, later
sibling/main progress, then exactly one ended/complete metadata record surviving
observer cancellation and owner stop. Keep those assertions unchanged. No
production file, source checkpoint, VM or socket was changed or run here.

POLICY_READ implementer: full current canonical AGENTS.md, development-rules.md
R01–R16, development-evidence.md, CLAUDE.md, managed-runtime-plan.md and current
`/Users/adalundhe/g121-handoff.md` reread at main
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`. Policy revision `f5cec365`; rules
SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Owned path: this document only, branch `p/managed-terminal-proposal`; prior e163
and 82425 branches preserved. Research skill's primary-source method used without
children, following the explicit coordinator/implementer role restriction.

## Concrete source path and authority

At the pinned main revision:

1. Trusted guest `Capture::read` emits canonical End on an actual zero-byte pipe
   read, or Aborted(errno) on a native read error. `Record` and `StreamState`
   validate the channel payload offset and terminal transition. These are stream
   facts; pipe EOF alone does not establish child wait status or population reap.
   [Capture::read](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/init/src/capture.rs#L599),
   [canonical ABI](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/abi/src/run/managed.rs#L334).
2. Daemon `bridge` verifies the image label and the actual stream state, but
   reduces both canonical terminals to `After::End`. On successful producer
   submission, `send_credits` calls `Output::finish`; it sets Life to Draining.
   [bridge/accept](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L941),
   [send_credits](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L856).
3. Remote `Control::finish` marks finishing; the final application Consumed
   control changes AwaitingAck to Finished. `flush` then requests QUIC FIN.
   Finished is distinct from Aborted even when accepted equals consumed.
   [Control](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L439),
   [consumed/flush](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L770).
4. Service::done depends on Subscription::Closed. A normal final-record ACK/FIN
   request does not itself assign that subscription state; disconnect returns to
   Listening. Output publishes Ended only when endpoint.run returns and Life is
   Draining with equal progress counters. Owner abort otherwise publishes
   aborted/unknown. This source sequence motivates the live test; its actual
   outcome remains unverified here.
   [Service/Endpoint](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L621),
   [Output/Life](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L149).

The reserved `MANAGED_ABORT` control can revoke one member independently of
capture credit. `MANAGED_CONTROL_END` ends that control scope; it is not a
per-channel normal End or an observer ACK. Preserve this separation and the
trusted run/member/channel/generation route.
[guest control queue](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/init/src/capture.rs#L43),
[control reader](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/workload/capture.rs#L82),
[daemon member abort](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L472).

## Pinned transport facts

RFC 9000 distinguishes requesting FIN, acknowledging all transmitted stream
data, and delivering received bytes to an application. Receiving FIN determines
final size; complete ordered receipt and application reading are later, distinct
facts. Connection close/reset does not substitute for successful stream
delivery. None establishes application durability or process reap.
[RFC 9000 §§2.4, 3.1–3.2, 4.5](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.1).

The actual Cargo pin is hyper-raft
`233b7fcef7a363327c5439b191724a38065062cd`. Its Send::finish sets FIN pending
and initially unacknowledged; Send::ack requires the FIN acknowledgement and
fully acknowledged pending bytes. Its public Finished event also mentions
stopping, so it cannot independently certify clean application consumption.
[pinned send implementation](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/send.rs#L48),
[pinned stream API/event](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/mod.rs#L373).
Shards' `Io::finish` only delegates that request; its driver currently forwards
Readable/Writable/Stopped but no Finished handler callback.
[driver](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L398).

The authenticated owner's Consumed control is a separate application protocol
acknowledgement of an exact contiguous **wire** cursor. It is not proof of disk
sync, subsequent business processing, or honesty beyond the authorized peer's
claim. Canonical Record offsets count payload bytes; remote Progress counts
encoded record bytes. Preserve both meanings. Terminal metadata's current
writeln is not synced or a durability guarantee.
[remote ACK predicate/Progress](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs#L249),
[metadata writer](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/managed.rs#L211).

## Small candidate, only if the actual fixture reproduces

Keep daemon Output::life the single owner of its logical capture outcome.
Retain Serving/Draining/Ended/Aborted, but carry a typed canonical terminal
`End` or `Aborted(errno)` in Draining instead of losing that distinction in
After::End. Derive a typed finished/aborted observation directly from the existing
remote Shared slot through Control; do not infer it from counter equality, add
a second terminal ledger, or cache copied progress. Reuse Shared notifications
even when capacity is zero: the wake only signals the existing credit loop.
Replace its premature Ended credit slot with an explicit Draining credit phase;
`Credits::next` waits on the existing condvar until Control's derived terminal
predicate is ready, then returns a completion action. Commit Life and write
metadata from that existing worker after dropping remote/credit locks. This
adds no worker, queue or copied terminal outcome: the credit phase owns grant
closure, remote Shared owns transport progress, and Life owns logical outcome.
The notification must synchronize with the credit predicate's mutex to avoid
a lost wake; it must not periodically probe Control or make Draining immediately
actionable in a loop while awaiting its ACK.

| Current Life | Authoritative event | Legal next Life and metadata |
| --- | --- | --- |
| Serving | Validated canonical End successfully submitted; request finish | Draining(End); no terminal metadata yet |
| Serving | Validated canonical Aborted(errno) successfully submitted; request finish | Draining(Aborted(errno)); retain the cause |
| Draining(End) | Remote typed Finished, meaning final record application-ACKed | Ended; append ended/complete once, with existing accepted/consumed cursors |
| Draining(Aborted(errno)) | Remote typed Finished | Aborted; append aborted/unknown once, retaining canonical failure cause |
| Serving or Draining | Owner/member revocation, failed finish or typed endpoint failure before completion | Aborted; preserve known accepted/consumed, tail unknown, revoke credit and stop transport |
| Serving or Draining | Observer disconnect without owner revocation | No logical terminal transition; existing retained record/cursor reconnect behavior continues |
| Ended or Aborted | Owner stop/drop, endpoint return or duplicate completion notification | Logical terminal remains unchanged; no duplicate metadata, but owned transport must still stop |

Ended/complete would mean the trusted canonical EOF record is application-ACKed.
QUIC FIN delivery remains a distinct transport fact; the actual fixture must
still observe EOF and exact bytes. Endpoint liveness is not the authority for
that logical outcome. Retain the existing endpoint ownership until owner cleanup
for this minimal candidate; earlier resource retirement would need separate
evidence and design. One member's terminal does not stop its sibling or main.

Implementation constraints to preserve at these actual call sites:

- Mark Draining before issuing finish; release the Life lock before calls that
  synchronously notify. Shared::notify invokes the existing callback inline;
  that callback must not acquire Life or perform metadata I/O. Completion and
  revocation serialize through Life; a terminal winner is sticky. Remote notify
  already drops Shared before waking, so the credit predicate can inspect the
  derived remote state without reversing that lock order. Never wait for an ACK
  while holding a state lock.
- Separate “publish logical abort if still nonterminal” from “stop this owned
  transport.” Output::abort currently returns early for Ended/Aborted. An earlier
  Ended publication must not bypass control.abort/request_stop and strand its
  endpoint thread. Endpoint return becomes an idempotent observation/cleanup,
  not an unconditional new abort that overwrites a proven canonical EOF.
- Keep remote Progress authoritative for existing wire offsets, including on
  revocation. Do not reset a known cursor to zero or call an unacknowledged
  terminal complete. Keep the canonical Aborted errno typed through the handoff.
  Terminal metadata is emitted only by the Life transition winner.

These details are proposal obligations, not an assertion that a race or resource
leak was reproduced. Typed error propagation and exact metadata-write failure
handling must be reviewed with any authorized implementation; do not conceal a
failed write as durable publication.

Alternatives considered: retain the current behavior if root's fixture misses;
or end the driver as soon as a final record is ACKed. The latter couples logical
completion to endpoint lifetime and risks discarding FIN retransmission state;
the existing driver's `done && out.is_empty()` condition is not a peer-FIN-ACK
barrier. Publishing Ended from raw transport exit/equal counters also loses
canonical failure provenance. The candidate instead uses existing completion
notifications and explicit terminal causes, with no timer, polling, cache,
parallel manager, new numeric limit or platform-specific adapter.
[driver exit condition](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/net.rs#L638).

## Gate and retained identities

Root must first retain exact source/artifact identities, commands, canonical End
and ACK/EOF observations, metadata before/after observer cancellation and owner
stop, sibling/main progress and cleanup from the actual fixture. If that misses
the predicted mismatch, reject or revise this candidate using the real result.
After any separately authorized change: independently remove canonical-terminal,
ACK and sticky-outcome guards; observe wrong outcome/state or missing cleanup,
restore exact source and rerun. Include withheld ACK, disconnect/reconnect,
canonical Aborted, revocation during drain, endpoint failure, duplicate events
and owner-stop-after-completion. Root owns all such campaigns, combined target
checks, actual endpoint-thread/resource return and durability/process-reap claims.

Inspected source identities (SHA256; all Shards files at main24f):

| File | SHA256 |
| --- | --- |
| shards daemon/managed.rs | `524751057d4466b382c5fca303e2487f1c6898d2a3532b19722fc6e21ae8a44e` |
| remote managed.rs | `eeb45ad4e8a7fd2fcaaff293bede87b2adc99e37cd5fa9f3d036da2d7f6d852f` |
| remote net.rs | `e3cf92dbff95d0302b96199d170ade037361585bf425a766a226ce09d8f0c678` |
| abi run/managed.rs | `53dc711b4d434252b5d4cc6b136ee1244656d3201e90850e24b42c1c8c29cd01` |
| init capture.rs | `ade26d0b9762e61272244f6413ea345672a1f1c2f151e92c6c9842a9ecf7e763` |
| shards workload/capture.rs | `3d1efdb8441109ed97fa94b7e306976e41327091137f18dc5c4f65adab71b4fe` |
| pinned hyper-quic streams/send.rs | `590c706d33133a3be372547755d284a032a91462d4d56c15adce039684745f80` |
| pinned hyper-quic streams/mod.rs | `50e24b8633c979304ef3de89bb20c659a618c1e8e1ef785fad942b33f69eba70` |

Read-only inspection was limited to the named seam, its concrete callers and the
pinned first-party transport sources. No builds/tests/campaigns ran; policy and
diff checks apply to this documentation checkpoint. R01/R04/R06/R07/R13 are
future implementation obligations; R02/R03/R05/R12 have pinned sources,
alternatives and a minimal existing seam, with no new constant/wait/cache.
R09/R10 require root's still-pending live proof before any fix. R08/R14 cost,
allocation/copy/fault and comparative measurements remain unrun/root-owned.
R11 portability and real hardware remain unverified by prose; macOS support is
arm64 only and Windows ARM stays paused. R15 limits this lane to the proposal
and signed checkpoint, with no children/main merge. R16 preserves prior branches,
evidence and protected targets, with no local build or new worktree.
