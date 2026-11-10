# Four named VMM queue maxima: contracts and root measurement requirements

POLICY_READ implementer `01a1c5bda0bf766683384c0705c0a9bc`, coordinator
`01a148884a4770fdaf0111f3f792a248`. Canonical R01–R16 remain at full SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Full current managed-runtime plan read at main
`7c26e5292161fc891a7c6c6525305464a3a044be`, SHA-256
`ebae0bb6922dba5f90d036d06310cc546352279fd368b5c307770a84ddc4a968`;
full current handoff read at SHA-256
`42f035652d31893551dba0fe9339c75cf4a289377cf3d0ee4a9c0302a85f9cfb`.

Root named exactly four decisions: blk/fs/GPU control maxima 256 and IOMMU
requests 64. This is focused primary-source research and an **unrun** root
measurement proposal, not a repository audit, selected depth/algorithm,
production fix or performance result. The source-derived
[caller map](next-seams.md) still has pending coordinator confirmations,
including the precise `vmm-round2` evidence locator/revision and whether FS's
shared literal is to be considered for both affected queues. Both uses must be
read to avoid misrepresenting the existing literal; no change to either is
authorized here. A targeted path-name query found no matching evidence locator
at main3ea, which is not proof the evidence is absent elsewhere.

Nine named Shards source blobs are unchanged from base
`24f48ee5ebf198485117aa93bf47685ad7d7d53f` through main7c26. The
[source ledger](vmm-rings-source-identity.json) records those blobs, thirteen
external raw-byte identities, connector Git blob matches, URLs, retrieval times
and the first identity attempt's safe refusal when the canonical plan advanced.
No source tree/archive, target, runtime job or new worktree was created.

The [exact Kiln research export](vmm-rings-kiln-research.md) retains citations
and automated grades: block B/72, FS B/72, GPU B/79, IOMMU B/79 and the proposed
Little's-law interpretation C/68; all twelve cited quotes were found. Grades
are not semantic certification, runtime evidence or a sizing criterion.

## Shared representation and lifetime constraints

`VirtioDevice::queue_max_sizes` advertises maxima; the driver's configured N is
validated by `Queue::new`. These are split virtqueues, with nonzero power-of-two
N no larger than the advertised maximum and 32768. A descriptor can be one
element of a chain; an available head is a buffer/request, not necessarily one
descriptor. Bus setup chooses/configures N; it is not measured by reading the
maximum. [Shards trait](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/mod.rs#L163),
[validation](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/queue.rs#L301),
[Virtio 1.2 CS01 §2.7](https://docs.oasis-open.org/virtio/virtio/v1.2/cs01/virtio-v1.2-cs01.html).

The source validates descriptor, available and used extents of `16N`, `6+2N`
and `6+8N`: their sum is `26N+12` bytes. This is an extent sum, **not** actual
guest allocation, page rounding, driver metadata, indirect tables, data pages,
host/native/backend retention or hard aggregate charge. Alignment, feature
negotiation and driver allocation add constraints. `ChainBuilder` also bounds
the whole expanded chain, including an indirect table, by N. A candidate cannot
change advertised queue size independently of the requests/segments it permits.
[Checked extents and chain traversal](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/queue.rs#L209).

Linux's pinned split-ring add path uses one main-ring descriptor when an
indirect allocation succeeds; otherwise it needs the complete scatter/gather
count. ENOSPC means outstanding work occupies needed descriptors and retry can
follow completions; ENOMEM and broken-queue errors are distinct. Negotiating
indirect support alone does not prove each request consumes one descriptor or
that allocation cannot fail.
[Linux v6.18 ring add](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/virtio/virtio_ring.c#L533).

The shared worker serves before its first wait, returns at most one negotiated
ring's worth of requests per `serve_round`, checks stop between requests,
publishes used entries, re-arms notification with a barrier/recheck, and parks
only when idle. Stop waits for the current request/worker. Thus queue size also
influences round work; one slow handler can dominate wall time regardless of N.
These are source semantics, not a bounded stop-latency or fairness result.
[Round/re-arm](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/queue.rs#L109),
[worker ownership](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/worker.rs#L22).

The normal MMIO activation validates against today's advertised maxima. Restore
instead preserves the saved driver's queue configuration up to the representation
maximum, because that driver does not renegotiate a newly advertised smaller
ring. Admission/cost must therefore use the actual restored N, not blindly the
new maximum. PCI construction uses the same device/transport queue machinery.
Do not reinterpret a live queue's size as an adjustable batch counter. Queue
configuration changes require the appropriate stopped/reset protocol; no new
reset feature or live-resize behavior is proposed.
[Activation/restore](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/mmio.rs#L187),
[Virtio 1.2 CS01 §4.2.3.1](https://docs.oasis-open.org/virtio/virtio/v1.2/cs01/virtio-v1.2-cs01.html).

## 1. Block request queue, advertised 256

Shards has one block queue and advertises indirect descriptors. Its separate
SEG_MAX is 254, described as data segments excluding header/status; `handle`
executes one chain and returns status/used length. A reduction of the queue
maximum alone is not a coherent segment/chain compatibility method. Read-only,
flush/error and pause/reset/restore behavior must remain intact.
[Literal/config/features](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/block.rs#L24),
[worker/handler](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/block.rs#L770).

Linux `virtblk_add_req` supplies an outgoing header, optional data SG list and
incoming status. Its default tag queue depth starts from actual `num_free`,
halved without indirect support; an explicit module parameter overrides that
choice. ENOSPC stops the hardware queue, whereas ENOMEM is distinguished.
Completion drains used buffers with callback re-arm/recheck and restarts stopped
queues. The advertised maximum, tag depth, descriptor occupancy and outstanding
logical I/O are therefore separate observations.
[Linux request/refill/depth](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/block/virtio_blk.c#L139),
[completion](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/block/virtio_blk.c#L350),
[depth](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/block/virtio_blk.c#L1468).

Root's individual case must identify backend, request sizes/SG distribution,
read/write/flush mix, actual feature/N/tag configuration and data-integrity
witnesses. Compare arrival→publication→service→used→driver-reclaim boundaries;
include a slow owned backend, ring-full refill, indirect allocation fallback and
stop/reset with in-flight work. Root chooses finite inputs and failure methods.

## 2. FS shared maximum 256, two affected queues

`queue_max_sizes` returns `[256,256]` for hiprio and the sole request queue.
Each gets its own worker; the first is exempt from the ordinary I/O limiter.
The existing source shares the literal, but this does not derive equal demand,
equal reservation or an equal future size. Scope for changing both remains
pending root confirmation.
[Shards queues/workers](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/fs/mod.rs#L822),
[advertisement](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/fs/mod.rs#L1250).

Linux constructs request/reply SG lists and possible bounce/temporary arrays;
it retains/retries queued requests on ENOSPC and schedules dispatch after used
completion. FORGET uses the hiprio path. At mount, the actual request vring size
must exceed FUSE header overhead and also limits `max_pages_limit`. Therefore a
queue change can affect request splitting/size as well as concurrency/memory.
[Linux enqueue/refill](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/fuse/virtio_fs.c#L1370),
[completion](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/fuse/virtio_fs.c#L810),
[mount/page limit](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/fuse/virtio_fs.c#L1682).

The separate hiprio queue exists so interrupt/forget work can proceed while
normal queues are full; the device must not pause it because of normal-queue
activity. This is a contract to preserve, not proof the current implementation
meets every progress case.
[Virtio 1.3 CSD01 §5.11.6.2](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html).

Root must separately witness request data/metadata operations and hiprio
completion under a held normal request, actual N/descriptor/queued-request/page
counts, SG/bounce costs and error/teardown ownership. An idle mount/boot is not
an occupancy, request-limit or independent-hiprio progress result.

## 3. GPU control maximum 256, cursor excluded

Shards advertises `[256,1]`; only control index 0 is the selected decision.
Its GPU round first handles renderer signals, then at most control N requests,
publishes available completions and services cursor separately. A fenced answer
can remain in `held`/timeline waiting state until a signal permits used return.
Consequently parsing a command or obtaining a response is not necessarily ring
completion or backend/GPU resource release.
[Shards literal](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/gpu/mod.rs#L50),
[fenced retention](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/gpu/mod.rs#L241),
[round](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/gpu/mod.rs#L566).

Linux builds command, optional data SG and response descriptors, treating the
main-ring need as one when indirect support is present. It waits for enough free
descriptors, queues the command and processes used responses/fence events before
reclaiming buffers. Driver availability, command completion, fence completion and
native renderer memory are different quantities. The KMS setup identifies the
two queues independently; no cursor-depth recommendation is made.
[Linux queue/wait](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/gpu/drm/virtio/virtgpu_vq.c#L371),
[SG construction](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/gpu/drm/virtio/virtgpu_vq.c#L454),
[response/fence processing](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/gpu/drm/virtio/virtgpu_vq.c#L224).

Root must name an actual supported renderer/device/feature set and distinguish a
pure protocol fixture from native GPU work. Required witnesses include held
fences, actual signal/used/reclaim, driver wait/refill, sibling/control progress,
native resource return and the existing snapshot eligibility/refusal contract.
No rendering/host-driver change or synthetic-renderer performance claim follows.

## 4. IOMMU request maximum 64, event ring excluded

Shards advertises `[64,1]`, retaining the event queue separately while requests
use the shared worker. **Its features do not include INDIRECT_DESC.** Linux's
request helper supplies one outgoing plus one incoming SG. The shared split
ring therefore needs two direct descriptors for such a request, giving the
source-derived bound `outstanding requests ≤ floor(N/2)` for actual negotiated N.
The advertised 64 must not be called 64 concurrent requests. This is an inference
from exact source, not measured occupancy.
[Shards features/queues](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/devices/virtio/iommu.rs#L905),
[Linux request](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/iommu/virtio-iommu.c#L218).

Linux can batch adds without kicking/waiting; on ENOSPC it synchronizes existing
requests and retries. The sync path kicks then reclaims until its request list
is empty; synchronous callers also inspect the returned status. This couples
depth to batching and guest CPU waiting. Its separate event queue is prefilled
from free entries, so event-buffer allocation is a distinct cost, not evidence
for request depth.
[Linux synchronization](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/iommu/virtio-iommu.c#L150),
[event refill distinction](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/drivers/iommu/virtio-iommu.c#L1123).

Root's bounded case needs actual supported mapping/probe/attach requests and
status/translation effects using existing permitted device APIs, not a new host
privilege/security mechanism. Witness batched/full/synchronous paths, descriptor
reclaim, guest CPU wait, worker stop and reset/restore ownership. No IOMMU
translation/authority feature change is proposed.

## Established implementation comparison, not selection

QEMU v9.2.0 resolves to `ae35f033b874c627d81d51070187fbf55f0bf1a7`, dated
2024-12-10. Its named implementations illustrate different configurable/default
choices. They are historical mechanism comparisons, not latest versions,
equivalent workloads or measured competitive baselines:

| Named implementation | Source fact and reason not to copy it |
| --- | --- |
| Block | `queue-size` defaults to 256 with explicit representation/minimum checks; segment maximum can follow queue size, and coroutine-pool accounting also uses queue count/size. The number alone omits those contracts. [Pinned QEMU](https://github.com/qemu/qemu/blob/ae35f033b874c627d81d51070187fbf55f0bf1a7/hw/block/virtio-blk.c#L1846). |
| FS | Configurable queue-size defaults to 128 and feeds both hiprio and request queues; the vhost-user server/topology has separate ownership/cost. This is not a Shards sizing derivation. [Pinned QEMU](https://github.com/qemu/qemu/blob/ae35f033b874c627d81d51070187fbf55f0bf1a7/hw/virtio/vhost-user-fs.c#L239). |
| GPU control | Control depth is 256 in virgl mode and 64 otherwise; this depends on a feature/mode choice. No new value follows for Shards. [Pinned QEMU](https://github.com/qemu/qemu/blob/ae35f033b874c627d81d51070187fbf55f0bf1a7/hw/display/virtio-gpu-base.c#L196). |
| IOMMU requests | QEMU uses its 256 default for both request and event queues. That does not establish the correct allocation, outstanding-request count or batching policy here. [Pinned QEMU](https://github.com/qemu/qemu/blob/ae35f033b874c627d81d51070187fbf55f0bf1a7/hw/virtio/virtio-iommu.c#L1415). |

The Shards GPU comment also names libkrun; this batch has not independently
checked that implementation and does not certify the comment's equivalence.
Linux v6.18 is a deliberate driver pin, not asserted to be root's experimental
6.18.48 running kernel. OASIS documents are explicitly 1.2 CS01 and 1.3 CSD01;
the latter is a draft, not asserted final/current. Hashes identify the exact
read bytes; HTTP modification dates are not their stated publication dates.

## Alternatives and the limited role of Little's law

Little's original publisher abstract states conditions for equality of **means**
L = λW. His later author reprint distinguishes long-run queue averages from
finite-interval behavior. This is not a bound on worst occupancy, bursts, tail
latency or descriptor capacity. The original full proof was not obtained here;
the accessible publisher abstract and author reprint are the primary sources.
[Little 1961, DOI](https://doi.org/10.1287/opre.9.3.383),
[author's anniversary paper](https://projectproduction.org/journal/reprint-littles-law-as-viewed-on-its-50th-anniversary/).

For a root-selected observation boundary, measure effective admitted arrivals
and matching residence until departure, not offered work that was refused.
Requests, descriptors, bytes, tags, held fences and backing allocations require
different units. Service-only time omits queueing/reclaim delay. A finite trace
must disclose work already present at its start or remaining at its end; an
occupancy-area/residence comparison can check consistency but supplies no
distribution-free capacity guarantee. These are proposed analysis requirements,
not an executed estimator or production learning rule.

| Conditional alternative | Derivation/cold-start requirement | Failure and remaining evidence |
| --- | --- | --- |
| Keep current maxima, establish a complete cost/demand model | Separate actual negotiated N, driver admission/request-size contracts and active/restored topology. Use source bounds plus root data for reachable peaks, not just ring bytes. | No hard-memory/throughput/fairness conclusion from the existing literals. Cold guest/host allocation and partial setup/refusal/return must be observed. |
| Pre-activation size from actual admitted work/resource authority | Derive a representation-valid maximum from supported request/SG demands and explicit owner budgets/concurrency, preserving segment/page/protected-control requirements. A benchmark number is only validation of that method. | Requires actual authority inputs and a justified cold rule; missing knowledge is an explicit refusal/uncertainty, not a copied default. No live resize or arbitrary minimum is introduced. |
| Authority-bounded demand learning at a legal configuration boundary | Evaluate measured arrival/residence/backlog and tail/burst uncertainty inside admitted resources; change configuration only through a proven supported initialization/reset contract. | Current queue size cannot be rewritten while active; saved driver/ring state and native backend holdings remain authoritative. No persistent profile/cache, generic manager or reset feature is proposed. |
| Address a measured service/notification bottleneck separately | If root reproduces an actual issue, compare existing completion/re-arm/round/backend behavior before concluding advertised depth is its cause. | More descriptors can increase outstanding memory and wait/round work without increasing service rate. No guessed worker optimization or fairness fix is authorized. |

## Root-only instrumentation and execution matrix — entirely unrun

First resolve the evidence locator/caller/FS scope questions and name the exact
candidate or baseline fixture, source, kernel/config/driver, backend, features,
admitted resource envelope and finite caller inputs. No runnable new harness is
approved in this lane yet; this record does not invent commands or execution.
Any implementer harness work requires a separate named-path approval. Root runs
all profiling, VM/backend cases, failures, mutations, comparisons and sweeps.

| Required root case | Independent witnesses and acceptance |
| --- | --- |
| Cold/fresh/restored identity | Read actual guest-configured N/features and driver tag/page/request limits, then independently match host activation and saved/restored state. A new smaller advertised maximum must not silently reclassify a saved larger ring or its ownership. Refused setup releases acquired queue/worker/backend state. |
| Descriptor/request accounting | Match authoritative publication, pop/service, used and driver-reclaim events with actual direct/indirect SG costs. Include indirect allocation refusal/fallback where supported. Wrap/index epochs and queue generation prevent mistaken head reuse or double counting; do not add a second authoritative in-flight cache. |
| Empty/full/wrap/notification | Use finite caller-controlled request sequences across empty→nonempty and full→reclaimed states, wrap and publish-during-rearm. Independently witness actual completed device effects/status and used/reclaimed buffers; a kick/log alone is insufficient. Keep memory barriers and initialized extents intact. |
| Per-device boundaries | Block: exact read/write/flush/data/status and SG/segment compatibility. FS: request splitting/page limit plus independent hiprio while normal work is held. GPU: actual supported renderer, held fence→signal→used/reclaim. IOMMU: supported mapping/status effects, two-direct-descriptor occupancy and full-queue sync/retry. No security/host privilege change or synthetic-equivalence claim. |
| Cold/overflow/failure/cancel | Checked extent/size/address/chain rejection and fallible guest/host/backend setup; malformed chains and driver errors; allocation refusal; slow/failed backend, stop/reset/pause at held work, restore and native cleanup. Witness actual outcomes/resource return before asserting. Stop between requests does not promise interruption of a blocked backend call. |
| Control/neighbor progress | A bounded busy queue/refill source with independent quiet/control work, owned cancellation and original absolute deadlines. Measure completed control/neighbor effects and CPU/time spent in handler/round/guest wait. No arbitrary latency allowance or starvation claim. |
| Occupancy/model consistency | Record starts/ends, admitted rate, in-flight request/descriptor/backing occupancy and residence at the same boundary; disclose incomplete/censored work, bursts, workload shifts and confidence. Compare empirical occupancy area with matched per-request residence; means alone cannot choose a safe maximum. |
| Candidate counterfactual | After exact root-approved basis only, independently remove its actual representation/admission/re-arm/ownership guard and witness the corresponding real forbidden effect, loss, hang or excess retention. Restore exact source and run a fresh closing case. A changed log or a different failure is not the witness. |
| Paired costs and portability | Use equivalent pinned guests/features/backend/security and controlled workload; preserve raw caller n and p50/p90/p99/max, CPU, allocation/reallocation/copy, guest/host/native/kernel/parent retained cost, faults, throughput, request/control/neighbor latency and misses. Compile the relevant seven targets and run actual supported backends; unavailable hardware remains unverified. No 4–5× claim or constant adoption from one run. |

## R01–R16 checkpoint account

| Rule | Actual evidence and remaining limit |
| --- | --- |
| R01 | Production unchanged. Size/chain/setup/error paths read; no panic/OOM/refusal proof or whole-repository review claimed. |
| R02 | Exact local/Linux/QEMU/OASIS/primary-author sources and alternatives retained; live data and selected method absent. |
| R03 | 256/64 classified as advertised maxima, not invariants or measured occupancy; protocol bounds/defaults/request contracts separated. No replacement number/method. |
| R04 | Existing activation/notification/completion/stop/reset/restore and per-driver retry/fence paths identified; actual transitions/effects unrun. |
| R05 | Existing device/queue/worker seams only, no second in-flight cache, speculative abstraction or new manager; source-only alternatives conditional. |
| R06 | Exactly four named decisions and necessary shared callers; no inventory/audit sweep or complexity claim. |
| R07 | Citations explain constraints, source limits and proposed witnesses; runtime comments untouched. |
| R08 | Historical QEMU mechanisms are not comparable performance data; ratios/distributions/misses remain root work. |
| R09 | No source observation reclassified as a live defect or license to fix. |
| R10 | Durable identities, failed freshness attempt and bounded unrun cases retained; before/after/mutations/cleanup owed. |
| R11 | All compile/real-backend coverage unrun; macOS arm64 only, paused Windows ARM untouched. |
| R12 | Shared seams preserved, no OS adapter, host/root/admin/security/IOMMU authority or crypto change. |
| R13 | Focused research; no new architecture, feature or generic learning/cache framework. |
| R14 | No profile/measurement/benchmark executed; induced guest/parent/native/kernel costs and faults are owed. |
| R15 | Root owns audit/execution/review/main merge; original lane continues signed research checkpoints, no explicit child spawn, main merge or maintenance completion. Existing automatic-session conflict remains reported. |
| R16 | Mac dipped below30GiB then user restored33,713,639,424B. No local build/active target/cache job/new full tree/mirror/archive/cleanup; no heavy builds/new full trees authorized. Source/research continues; all protected root validation paths/evidence and paused VM preserved. |
