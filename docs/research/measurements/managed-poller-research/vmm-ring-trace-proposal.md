# Finite ring trace: proposed event and acceptance contract

This refines the already approved four-ring measurement proposal returned in
`ba8ea1516ee8a9071e832c24d77608c8de2e2bb6`. It is a textual design for root
review, not an executable harness, selected runtime method or measurement.
No new production/test instrumentation, callback abstraction, in-flight cache,
kernel feature or execution is authorized by this record. Root must assign exact
harness paths and approve its source/resource/runner envelope before code work.
The original lane has requested the next scoped literal and continues this
specific unfinished event-boundary requirement while root reviews the return.

The [exact new Kiln finding/source excerpt](vmm-ring-trace-kiln-research.md)
retains B/79 and both found quotes for this inference. The automated grade is
not technical certification, a live consistency result or a sizing criterion.

POLICY_READ implementer; canonical rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`,
current fully read plan main7c26 SHA-256
`ebae0bb6922dba5f90d036d06310cc546352279fd368b5c307770a84ddc4a968`.
Scope, exact local/Linux/QEMU/spec pins, handoff42f readback and all unrun cases
remain in [the four-ring return](vmm-rings.md) and
[its identity ledger](vmm-rings-source-identity.json).

## Why one occupancy counter is insufficient

Use an actual queue generation and these **different** boundaries:

| Event | Authoritative observation seam to approve | Meaning; what it does not establish |
| --- | --- | --- |
| A: published | Successful guest descriptor/available publication in the named Linux ring add path, with actual main-ring descriptor cost. | The request occupies driver ring resources. Offered work or failed ENOSPC/ENOMEM attempts are not arrivals at this boundary. A preallocated tag alone is earlier state. |
| P: popped | Shards `Queue::pop`/`pop_into` successfully returns the chain to its selected worker. | Host service owns the popped request. Its guest descriptors are not thereby free. Invalid-chain dequeue/error must be a distinct outcome, not a successful P silently inserted into the cohort. |
| U: used published | The successful used-ring publication for that exact head and length. | The driver can observe completion. It does not prove the driver reclaimed descriptors or native GPU/backend resources ended. GPU can return previously held requests during a later signal/round. |
| R: reclaimed | Successful guest `virtqueue_get_buf` reclamation for the same request at the named driver's completion/sync path. | Main-ring descriptors become reusable. Application completion/status inspection and long-lived native resource ownership may be later/separate events. |

These seams are source-derived, not an instruction to insert hooks everywhere.
Root should first choose the smallest permitted owned fixture capable of
witnessing the needed boundaries. Host-only synthetic rings can validate P/U
and notification bookkeeping; they do not observe actual Linux A/R, tag/page
limits or native-backend cost. No missing boundary becomes passed by substituting
a log, a kick, renderer stub or different source's old result.
[Linux add/reclaim contract](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/virtio/virtio_ring.c),
[Shards queue](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/queue.rs),
[GPU held completion](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/gpu/mod.rs#L241).

For a complete, consistently cut trace, source event counts permit the
decomposition below, with initial occupancy included rather than assumed zero:

```text
published but not popped       = initial_backlog + A - P
popped but not used-published  = initial_host_held + P - U
used but not driver-reclaimed  = initial_unreclaimed + U - R
all published but unreclaimed = initial_total + A - R
```

Each expression counts requests at its own boundary. Main-ring descriptor
occupancy instead weights each unreclaimed request by its actual descriptor
cost. Indirect tables, data pages, tags, chain Vec capacity, host fence/timeline
objects and backend/native allocations are separate stored resources. Do not
sum shared backing more than once or infer all costs from a logical request
count. A successful allocation fallback can change descriptor cost.

The current IOMMU source-derived two-direct-descriptor bound belongs to A/R
occupancy. Its P/U service window is a different count. Block's driver tag depth,
FS's queued-but-unpublished requests/page limit, and GPU's held-fence state need
their own labels. This is postprocessed evidence from authoritative transitions,
not a proposed second runtime owner/cache.

## Identity, clock and bounded recording requirements

Root's concrete recorder proposal should specify:

* Exact build/ABI identity for all host and guest helpers, device/queue index,
  boot/reset/restore generation, negotiated N and features, backend/driver pin
  and the trusted fixture's bounded request identity. A recycled 16-bit head or
  index alone cannot identify a request across wrap/reset. Sparse snapshots of
  wrapping indices cannot reconstruct how many full laps occurred.
* Per-producer event order and clock domain. Guest and host timestamps cannot be
  subtracted or globally sorted as though synchronized. Measure guest A→R and
  host P→U within their own clocks, or establish a calibrated relationship with
  explicit error bounds and an independent causal cut. Restore/pause clock
  discontinuities create a new classified interval. A release/acquire handshake
  can establish ordering without fabricating simultaneous timestamps.
* A minimal bounded record: request/queue/generation identity; event kind and
  ordinal; actual descriptor/SG count and direct/indirect/fallback result;
  relevant length/capacity/retention class; success/refusal/error; producer clock
  and timestamp/uncertainty; configuration and terminal/cleanup references.
  Payloads, credentials and unnecessary raw addresses do not enter ordinary
  records. An application's reported identity is not admission authority.
* Caller-supplied finite work/event/deadline/storage budgets, with checked
  record-count/byte arithmetic and fallible provision before workload release.
  Account recorder memory/copy/CPU overhead separately. No hidden default event
  count, arbitrary sample interval or periodic probe is selected.
* An explicit `incomplete trace` outcome for event overflow, recorder loss,
  missing correlation, clock uncertainty, allocation refusal or interrupted
  cleanup. Required evidence space must be reserved before the fixture's
  corresponding effect where feasible. If observation is lost after a real
  effect, retain that uncertainty and the independent effect/cleanup witness;
  do not relabel the workload as safely refused or count the case passed.

No logging allocation/lock across actual device I/O is prescribed. A recorder
can perturb the exact queue it observes; root must compare the instrumented and
appropriate baseline behavior and quantify that uncertainty. Header/capacity
metadata checks or a preallocated Vec alone do not prove negligible overhead.

## Finite-window consistency, not a sizing estimator

The primary author gives finite sample-path identities with explicit definitions
and distinguishes them from long-run stationary versions. Those identities are
after-the-fact consistency relations, not forecasts of the next burst.
[Little, anniversary paper §§2.1–2.4](https://projectproduction.org/journal/reprint-littles-law-as-viewed-on-its-50th-anniversary/).

For one consistent clock/boundary, let request i occupy the system on
`[a_i,d_i)`, and choose a finite observed window `[s,e)` with `e>s`. Define
`q(t)` as the count of those intervals covering t. Then:

```text
occupancy_area = integral from s to e of q(t) dt
               = sum_i max(0, min(d_i,e) - max(a_i,s))
mean_occupancy = occupancy_area / (e-s)
```

This equality follows by integrating each request's membership indicator over
the window and adding the clipped residence durations. It needs complete
membership/censoring information at that boundary. For requests already present
at s, include their observed overlap; for requests still present at e, include
the observed overlap and mark unfinished residence. Unknown membership or a lost
departure means the equality is not established, not that the request vanished.

If root reports `arrival_count/(e-s)` multiplied by mean **completed** residence,
it must explain the cohort and boundary correction; that product generally does
not equal a finite-window time-average occupancy when initial/final work is
excluded. A fully drained, consistently defined cohort can provide a simpler
check. Offered arrival rate, service-only duration and mixed host/guest clocks
must not be substituted. Descriptor-weighted area needs the actual descriptor
weights; it is not the same unit as requests, bytes or retained backing.

Even exact finite-window equality supplies no future tail/maximum, necessary
headroom or numerical queue recommendation. Root still needs workload changes,
bursts, fallback behavior, uncertainty, owner authority and independent progress
requirements before approving a method under R02/R03.

## Decisive acceptance and counterfactuals — none executed

| Proposed check | Expected independent result; false acceptance to prevent |
| --- | --- |
| Head reuse/wrap/reset | Actual effect/status and A/P/U/R correlation remain unique within a queue generation. A trace that joins a new request to an old recycled head is rejected/incomplete. No duplicate in-flight cache is introduced to guess the missing identity. |
| GPU held fence | P can precede U until the actual signal; R follows actual used/reclaim. Removing fence retention must be detected by the real completion/order witness, not merely by fewer trace events. |
| Driver ring full/refill | Published-but-unreclaimed descriptor weights match actual free-descriptor changes; failed enqueue is not counted as A. Removing the actual admission/refill guard must produce the corresponding actual ownership/effect failure before it can be a useful counterfactual. |
| Missing/censored event | Dropping one evidence event must make the consistency result explicitly unknown/incomplete, not improve latency/occupancy. This tests only the recorder; it does not replace a production guard-removal case. |
| Clock mismatch | A supplied clock-domain mismatch is refused or its uncertainty retained. Negative/implausible cross-domain duration is not clamped into a fast pass. This does not validate actual cross-VM clock synchronization. |
| Stop/cleanup | Capture workload terminal effect, worker join, queue/backend resource return and the original deadline independently of a complete trace. An exhausted recorder cannot excuse lost cleanup or prove that no device effect occurred. |

Root must name executable source/artifacts and invocations for the approved
fixture before running these cases. All native/VM, fault/mutation, profile/cost,
hardware/target checks remain unrun by this lane. The [R01–R16 account](vmm-rings.md)
applies: no production change, no selected algorithm/depth, no cache/abstraction,
no explicit child spawn, no host-root/security/crypto action and no performance
or managed-plan completion claim. Latest user headroom receipt is
33,713,639,424B; no local heavy build/new full worktree and no active local
target/cache/job. Root validation sources/targets and paused Windows ARM remain
untouched.

The [trace source/check record](vmm-ring-trace-source-identity.json) pins the
parent research and supplies the correctly shell-quoted targeted evidence-name
query. The earlier ledger omitted the regex's quoting when rendering its command
string; its actual complete query/exit1 remains the observation. That textual
correction does not imply another search or an absence proof.
