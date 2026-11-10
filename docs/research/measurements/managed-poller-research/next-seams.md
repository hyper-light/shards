# Next constants seams: identity and ownership gate

This is the Step 3 scope record for plan
`01a1c5cfb0f6736f910c8f76dcbfc37f`. It resolves source identities, not the
derivations or runtime validation assigned to later steps. **Exact caller-scope
acceptance and the `vmm-round2` evidence locator remain pending ROOT's reply.**
No replacement constant, production change, architecture or measured result is
selected here.

## Policy and scope receipt

POLICY_READ implementer, Kiln session `01a1c5d8a67f740a80f296e5e9cfe86a`:
full `AGENTS.md`, `docs/development-rules.md` R01–R16,
`docs/development-evidence.md`, `CLAUDE.md`, managed-runtime plan, lane handoff
and `/Users/adalundhe/g121-handoff.md` read before these writes.
Started HEAD: `6eb84cf671278e140f8018768896f408324e0b28`. Freshly fetched
`origin/main` and inspected source: `24f48ee5ebf198485117aa93bf47685ad7d7d53f`.
The checkout's three policy files match that main revision byte for byte.
During this step, freshly fetched main advanced to
`096c9c7be1752a5eaf99eda2c7eb4723c2a8f6db`. All five policy/mechanics/managed-plan
files and all 13 named production/dependency files in this record are unchanged
from 24f. The coordinator-selected research revision remains explicitly 24f;
no newer source result or final caller-scope acceptance is inferred from the
main advance.
Current user steering explicitly confirms source/main 096c and unchanged policy
d634. The path/function map below therefore also describes those identical 096c
blobs; ROOT's final caller-boundary and evidence-locator answers remain separate.

* Rules SHA-256: `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
* AGENTS SHA-256: `5fd5c2bc14079994c7390926c8f3865efdfedf066faa9c33d7dc361697aeea28`.
* Evidence checklist SHA-256: `a84c60f20e3a06a294bd5b091c1f43cfd5cc4b33ef1bd22a50cd94cea07bb97b`.
* Handoff SHA-256 read this turn:
  `c561caf085fe38b685c0826ffda0776dcb709b8ceee4c0dd78da842efc1461ae`.

ROOT/coordinator is `01a148884a4770fdaf0111f3f792a248`; parent constants lane
is `01a1c5bda0bf766683384c0705c0a9bc`. This step writes only new textual
`next-seams*` records under the already owned
`docs/research/measurements/managed-poller-research/` directory. The parent owns
the TLS derivation. Step 2 is validating its existing Poller/inotify note and
manifest. Neither those files nor their checks/scripts are edited by this step.
The parent acknowledged this new-file split at `1794321045135` and retains new
`tls-handshake*` records for its own continuation. Step 2 explicitly released its
index writes at `1794321282452`, after signing/pushing
`2e50de388ff26f0c6e1f97fcb174440cda0f8104`. At `1794321308262`, the original lane
transferred the sole index window to this step for **only** `next-seams.md`,
`next-seams-source-identity.json` and `checks-next-seams.json`, and held its own
index writes until release. The acquisition receipt is
`managed-poller-research-step3-index-acquired-2e50-v1`.

This is a bounded WIP checkpoint, with ROOT confirmations still open. The current
user's serial scheduling correction forbids new children, `plans_ready`/`run`
and goal completion that could start more step sessions. This step performs none
of those actions and accepts no new/expanded assignment. Its exact signed commit,
push/remote-ref outcome and explicit index release go to ROOT and the original
lane in the checkpoint message after the operations actually finish. The
original lane continues TLS → QUIC independently in its already owned records.

Production remains read-only. No children, local build, new worktree/mirror,
cache cleanup, VM action or runtime fixture is started. The parent subsequently
relayed restored headroom of `33,299,759,104` bytes, with no heavy builds
authorized. This step starts no build or new storage-heavy action.
ROOT owns audits, profiles, benchmarks, real E2E, chaos, sweeps, combined gates
and main merges. macOS support is arm64 only. Protected ROOT sources/targets and
the paused Windows ARM VM are untouched.

## Coordinator selection and current source map

ROOT's actual queue message, timestamp `1794320032433`, selects daemon
`HANDSHAKE_BYTES`, remote managed `DATAGRAM_BYTES`/QUIC credit, then
virtio-blk/fs/GPU control 256 and IOMMU requests 64. It gives current main 24f,
research/ROOT-measurement scope, Linux driver request/refill contracts and no
production number selection. The message was recovered from the parent session's
accepted mailbox result, with sender ROOT; the identity manifest records its
provenance. These four device identities are coordinator-supplied, not inferred
from an inventory.

All source locations below are at exact main 24f. These source files are
unchanged between that base and starting HEAD 6eb. Sparse omissions are read with
`git show <revision>:<path>`; no checkout expansion is required.

| Queue item | Exact literal and defining path | Direct function/caller boundary identified in source |
| --- | --- | --- |
| TLS/QUIC handshake retention | `HANDSHAKE_BYTES = (1 << 24) - 1 + 4`, `crates/shards/src/daemon/managed.rs:28` | `Daemon::managed_capture` → `start_managed_capture`: per-active-channel reservation and `Admitted.handshake_bytes`; `Charge::{reserve,grow,drop}` retains/releases caller accounting. `crates/remote/src/managed.rs::transport` passes it to `TransportConfig::crypto_buffer_size`; `Endpoint::bind` and `client_config` use that transport configuration. This is a QUIC crypto-buffer setting, not a directly inspected rustls Handshake parser. |
| UDP datagram storage and separate credits | `crates/remote/src/managed.rs:86`, `DATAGRAM_BYTES = crate::net::DATAGRAM`; `crates/remote/src/net.rs:24`, `DATAGRAM = 65_527` | `Endpoint::bind` → `Driver::bind_bounded`; `Driver::{run,receive,queue_datagram,flush_datagrams,transmit}`. `transport` configures QUIC send/receive windows. `Producer::{admit,capacity,wait_capacity,submit}`, `Service::{subscribe,consumed,flush,disconnect}`, `Control::{finish,abort}` and daemon `send_credits`/`Charge` define the distinct application-credit/lifetime caller scope. |
| virtio-blk maximum | `crates/vmm/src/devices/virtio/block.rs:24`, `QUEUE_SIZE: u16 = 256` | `Block`'s `VirtioDevice::queue_max_sizes` returns one maximum. `Block::{activate,start,notify,stop,reset,pause,resume,drop}`; the worker calls `handle` for the request and returns its used-chain result. |
| virtio-fs maximum | `crates/vmm/src/devices/virtio/fs/mod.rs:49`, `QUEUE_SIZE: u16 = 256` | `Fs::queue_max_sizes` returns `[QUEUE_SIZE, QUEUE_SIZE]`: high-priority and sole request queue. `Fs::{activate,start,notify,stop,reset,pause,resume,drop}`; `start` passes each queue to a worker whose closure calls `serve_with`. **Whether the selected decision covers both queues needs ROOT confirmation.** |
| virtio-gpu control maximum | `crates/vmm/src/devices/virtio/gpu/mod.rs:53`, `QUEUE_SIZES: [u16; 2] = [256, 1]`, index 0 | `Gpu::queue_max_sizes`; `Gpu::{activate,start,notify,stop,reset,pause,resume,drop}` and `Running::{start,stop}` for the owned two-queue worker. Index 1 is read only to distinguish the cursor queue; it is not an additional selected 256 decision. |
| virtio-iommu request maximum | `crates/vmm/src/devices/virtio/iommu.rs:45`, `QUEUE_SIZE: u16 = 64`, index 0 | `Iommu::queue_max_sizes` returns `[QUEUE_SIZE, EVENT_QUEUE_SIZE]`; `activate` separates requests from events. `Iommu::{start,notify,stop,reset,pause,resume,drop}`; the worker calls `answer`. `EVENT_QUEUE_SIZE = 1` is only an identity distinction, not another selected decision. |

The shared semantic callers needed for these four queue choices are
`VirtioDevice::queue_max_sizes` in `devices/virtio/mod.rs`,
`MmioTransport::activate` and the queue-max register path in `mmio.rs`, PCI
construction of the same transport in `pci.rs`, `Queue::{new,pop_into,add_used,
enable_notification,disable_notification}` in `queue.rs`, and the actual
worker notification/stop path in `worker.rs`. Only these callers are proposed;
this does not authorize inspection of other device decisions.

`Queue::new` validates the guest's `QueueConfig.size` against the advertised
maximum and representation constraints. Thus 256/64 here are **advertised
maxima**, not evidence of actual negotiated size, occupancy, request count or
runtime allocation. FS uses two queues for one repeated literal. The four
selected *decisions* do not mean that the implementation has exactly four queues.
The restore path in `MmioTransport::activate` also distinguishes a saved ring's
configuration from a newly advertised maximum; later derivation must preserve
that caller contract.

## Datagram ambiguity resolved by the named source

`remote::managed::transport` explicitly sets
`datagram_receive_buffer_size(None)` and `datagram_send_buffer_size(0)`.
Consequently the selected endpoint is not using unreliable RFC 9221 DATAGRAM
frames. No RFC 9221 flow-control or frame-size conclusion is applied here.
The exact dependency's
[configuration setter](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/config/transport.rs#L322)
defines `None` as disabling incoming application datagrams. Its
[`crypto_buffer_size` setter](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/config/transport.rs#L307)
describes out-of-order crypto-layer buffering. That is the dependency seam for
the parent's TLS derivation; a uint24 Handshake field alone is not proof of that
buffer's total memory contract. These are source meanings, not a reproduced
runtime problem or a selected sizing policy.

The named datagram allowance instead reaches UDP receive scratch storage and
the driver's owned outbound UDP datagram queue. `Driver::bind_bounded` requires
room for `DATAGRAM`; `run` also derives a receive-turn count from that allowance.
`queue_datagram` counts retained buffer capacities, with checked summation and
fallible reservation, before retaining a queued item. These are source facts;
their completeness, cost and failure behavior are not live results.

Three different forms of credit remain separate:

1. QUIC stream/connection receive windows and transport send storage, configured
   by `transport` from `Admitted` and the framed data/control lengths.
2. One owned record's application consumption acknowledgment: `submit` changes
   `Ready` to `Record`, `flush` to `AwaitingAck`, and `consumed` accepts the exact
   next end offset before returning `Ready` or `Finished`. This is not merely a
   QUIC transport ACK.
3. Daemon/guest channel credits and caller-owned memory reservations:
   `send_credits`, `Producer::admit` and `Charge` are separate accounting and
   authority obligations. A buffer limit or window does not itself prove a hard
   parent/kernel reservation or a control-progress guarantee.

The actual dependency is `hyper-quic` from
`https://github.com/hyper-light/hyper-raft` at
`233b7fcef7a363327c5439b191724a38065062cd`, as declared in
`crates/remote/Cargo.toml`. The parent is tracing that pin for TLS. A registry
rustls package elsewhere in Cargo.lock cannot identify this call path by itself.

## Evidence expectations and open confirmations

ROOT's queue authorizes primary-source alternatives and concrete ROOT-owned
measurement/reproduction proposals, not execution or production implementation.
Each later item needs its exact source/dependency/spec pins; literal derivation
versus library default; cold-start, overflow/refusal, authority, retention and
cancel/teardown semantics; and an explicit list of unrun checks.

For TLS, ROOT owes actual multi-connection crypto-retention/admission and
fragmentation/refusal evidence. For UDP/credits, ROOT owes the packet/queue,
window and application-credit transitions, real backpressure, cancellation,
resource return and independent neighbor/control progress. For each named ring,
the later proposal must identify the matching Linux driver request/refill and
notification contract, actual negotiated ring/descriptor/request occupancy,
service time and arrival observations, completion/teardown witnesses and the
alternative derivation method. Little's law is a proposed method to evaluate,
not a production depth derived from a source constant. ROOT alone selects and
runs the actual bounded workloads, guard removals/restorations, comparative
costs and platform/VM gates. No such test is reported run by this record.

Open, requested with stable keys below:

* ROOT acceptance/correction of the exact function/caller boundaries above.
  Current source/main 096c is confirmed by user steering and unchanged named
  blobs; the initial selected revision 24f remains recorded for provenance.
* Precise committed path and revision of the referenced `vmm-round2` evidence.
  A focused path-name lookup and literal `vmm-round2` documentation lookup at
  24f found no locator. That is not proof that the evidence is absent elsewhere;
  no repository sweep, guessed path or invented result follows.
* FS scope: both high-priority and request queues share 256; confirm whether
  both belong to this one selected decision.

The earlier file/index-ownership dependency is resolved by the explicit Step 2
release and original-lane transfer above. Existing directory ownership is
retained; no broader writable source path is claimed. ROOT's confirmations are
not implied by that transfer or by signing this WIP checkpoint.

## Queue continuity and message evidence

The preceding goal turn made progress: signed checkpoint
`6eb84cf671278e140f8018768896f408324e0b28` added concrete capacity-preparation
contracts and the sustained plan. This step started from that actual clean
revision; it did not classify an intention or queue entry as running research.

The parent sent that checkpoint under
`managed-poller-research-checkpoint-contracts-6eb84-v1` at `1794320670641`,
accepted the next selected queue under
`managed-poller-research-next-queue-accepted-v1` at `1794320696400`, and actually
read the named TLS/managed source in tool `exec-17067631-34d7-4d51-b121-a1095fbba5ba`
at `1794320696687`. These conversation records show work proceeding to an
approved independent item after the checkpoint, without awaiting ROOT's review.
They establish source reading, not a completed TLS report or a running test.

| Stable key in this step | Purpose/status |
| --- | --- |
| `managed-poller-research-step3-parent-ownership-v1` | Policy/readback and shared-worktree ownership request, accepted for the parent's queue. |
| `managed-poller-research-step3-exact-seams-v1` | ROOT identity/caller/revision/ownership/evidence request, accepted for ROOT's queue. |
| `managed-poller-research-step3-quic-semantics-v1` | Source-backed UDP versus DATAGRAM/application-credit distinctions returned to the active parent lane. |
| `managed-poller-research-step3-step2-ownership-v1` | Shared file/index boundary sent to Step 2; its existing files remain untouched. |
| `managed-poller-research-step3-candidate-map-v1` | Exact six-item map, semantic callers and the remaining FS/evidence-locator questions sent to ROOT. |
| `managed-poller-research-step3-new-ledger-write-v1` | Parent notified of these new textual records and no shared-file/index mutations. |
| `managed-poller-research-step3-pinned-config-v1` | Exact upstream crypto-buffer and disabled application-datagram meanings returned to the parent, with the pinned source link. |
| `managed-poller-research-step3-step2-checkpoint-release-v1` | Requests explicit index release after observing Step 2 commit 2e50; no completion inferred from HEAD alone. |
| `managed-poller-research-step3-index-acquired-2e50-v1` | Confirms sole three-file index ownership to ROOT and the original lane after the two explicit handoff messages. |

The first attempts to send the first two keys with `mode=steer` were rejected
because this step did not start those recipients. The subsequent `mode=queue`
calls were accepted. Acceptance is delivery to a queue, not evidence that a
recipient read or approved the contents. No idle process, waiter or keepalive
was created. A zero-time mailbox read checked the specific live parent/ROOT
sessions while independent source work continued; it was not reported as
runtime validation or as work on any pending identity.

## R01–R16 and validation limits

The [existing lane account](README.md#r01r16-account-for-both-research-batches)
retains the prior batches' proof/gaps. For this source-identity-only batch:

| Rules | Applicable evidence and remaining limit |
| --- | --- |
| R01/R04/R06/R07/R13 | Documentation only; source state/ownership names distinguished. No new runtime panic/state/complexity path or production comment. Runtime safety proof is not supplied. |
| R02/R03 | Exact primary Git blobs and dependency pin; literals classified by actual callers. No chosen number/architecture, default-as-proof or measured claim. Later external alternatives and live data remain owed. |
| R05 | Only named seams and their semantic callers. UDP storage, windows, application credit and ring maxima are separated. No cache, duplicate state manager or polling fix is added. |
| R08/R09/R10/R14 | Durable identities and proposed evidence boundaries; no reproduced defect, mutation, allocation/fault/CPU/kernel cost, speedup or 4–5× result. All such work remains ROOT-owned and unrun here. |
| R11/R12 | Shared transport/queue/platform caller boundaries identified; no platform adapter/binary added. Seven-target and actual KVM/HVF/WHP coverage remain unrun for this record. |
| R15/R16 | Implementer-only scoped source/text work; no delegation/merge/campaign/cache mutation or local build. The explicit three-file signing window is recorded; commit/signature-header/remote-ref and index-release outcomes are returned after execution. ROOT review remains pending; protected resources and paused VM preserved. Serial correction forbids automatic next-step/goal-completion actions. |

Source hashing and documentation/policy checks are retained in
[checks-next-seams.json](checks-next-seams.json); they do
not prove the unrun runtime obligations. Step 3 remains active until the
outstanding scope confirmations are resolved or explicitly dispositioned by
the coordinator. Finishing this record cannot close the sustained parent lane.
