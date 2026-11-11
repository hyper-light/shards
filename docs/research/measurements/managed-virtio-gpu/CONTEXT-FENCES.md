# GPU-CTX-1: queued completion across context-ID reuse

Status: native component red/green reproduced; final strict VMM lib/tests Clippy
passed0 after the admission connection recovered. Earlier refusal is retained. No actual GPU
renderer, VM, mutation, fault campaign or performance result is claimed.
This follows the earlier source-only CONTEXT-RESEARCH.md investigation.

Source base5c614b520474e48594c7e012ed58502f57ead5b4; implementer
01a1c76784eb72ccaf9096c88182efb7, coordinator01a148884a4770fdaf0111f3f792a248.
Current0342 policy/hashdf2 loaded and ROOT acknowledged; full R01–R16 accounting
is in README.md. ROOT's approved split retains mod.rs resource/context/fence
core here; virgl/physical identity/selection/packaging belongs to Hex L2. This
change does not touch its constructors, Renderer trait or renderer/selector code.

## Exact component example

The existing native GPU worker test renderer queues an old completed fence while
its destroy callback returns. Actual virtio queues reside in GuestMemory. The
worker receives destroy, create with the same numeric context ID, and a fresh
fence on that context/ring/sequence. Before the correction, it handles all three
requests before the next callback drain; the old queued signal releases the new
request. The actual guest used index is5 instead of4 before the fresh fence is
signalled. This is a demonstrated component protocol/lifecycle error using the
existing cfg(test) renderer, not a demonstrated host GPU or guest-driver race.

`raw/red-context-fence-reuse`: baseline product plus the new native unit,
exit101,0pass/1fail/0ignored, exact wrong used index5vs4. Command:

```sh
lockf -t 0 /tmp/g121-mac.lock python3 docs/research/measurements/managed-virtio-gpu/light-check.py FRESH_LABEL cargo test --locked -j2 -p shards-vmm --lib devices::virtio::gpu::tests::a_recreated_context_waits_for_its_own_fence -- --exact --nocapture --test-threads=1
```

Apply the retained source.diff at its recorded base to reproduce the original
red product/test bytes; preserve the existing first checkpoint. The later fixture
strengthening pauses the worker while publishing the entire three-request batch,
then resumes it, so an incidental empty queue cannot drain the old callback early.
It verifies actual used heads `[0,2,4,6]` and the new request head8 still held,
then signals the fresh fence and verifies `[0,2,4,6,8]`. Its logging is an actual
used-ring witness, not a marker substituted for positive assertions.

## D-GPU-CTX-1: existing callback drain as a dispatch boundary

The private `Completion{used,next}` carries `Next::Request` or `Next::Signals`.
A successfully processed CtxDestroy returns Signals; its control round publishes
the used requests and yields More. The next worker round performs its existing
callback drain before another request can create a new context. Other contexts'
callbacks go through the unchanged signal path; no callback is discarded by a
new filter. Cursor service remains in the same round. No polling, deadline,
cache, persistent generation map, renderer manager or allocation is introduced.
Invalid context destruction retains its typed Refused outcome and does not
create the new boundary. Pause/resume remains exercised by the new unit.

Primary research: pinned VIRTIO1.3 CSD01 GPU timeline/context commands, Linux
v6.18 guest fencing, and virglrenderer1.3.0/krunkit0.10.4e sources retained in
context-source-receipts.json. Linux proxy_context_destroy stops and joins its sync
thread; the fork's context destroy stops rings and destroys its Vulkan instance.
The Linux driver's usual global monotonic fence IDs may differ from this valid
per-context reused sequence; no claim is made that its common traffic hits this
case. Actual remaining backend/device/sync teardown and callbacks arriving after
destroy are unverified and belong to ROOT's native GPU/VM gates.

Alternatives: purge the renderer's queued signals during its destroy (requires
changes in the other lane's renderer state and conformance coverage); drain after
every request (adds repeated synchronization work); carry new context generations
through the callback ABI (larger state/interface change). The explicit context
transition boundary fixes the exact observed queued-signal sequence using the
existing authoritative worker drain, without changing the renderer contract.
No timing/resource win is inferred from source or the test.

## Results, failure preservation and current hold

All native executions use the private Mac target, lockf,30GiB floor,
2CPU/2GiB/4GiB/600s/-j2, exact product lock1ecb41e7...:

| Record | Actual result |
| --- | --- |
| red-context-fence-reuse | baseline component used5instead of4;101,0/1/0 |
| green-context-fence-reuse | minimal boundary correction;14/0/0 |
| green-context-batch-proof | stronger fixture;13/1/0. Its new used_heads helper nested GuestMemory::access through used_idx and received typed Reentered. This is a fixture failure, not a new product defect. |
| green-context-batch-guard | corrected helper reads the count before taking its single access guard;14/0/0. Actual printed head sequences above. Original assertions and deadline retained. |
| attempted final strict Clippy | Kiln refused before launch: could not ask whether the session may act because it was not connected to the Kiln server. No lint job/result exists; admission is not bypassed with local direct execution. |

The initial fixture-strengthening apply_patch had a context mismatch and applied
nothing; exact current spans were read and the patch applied. That and all test
failures are retained in raw/context-attempt-notes.txt. No timeout was enlarged,
positive assertion removed, event fabricated in production, or fault/mutation
campaign launched. Final policy/rustfmt/production diff checks pass0. Normal signing is used; it
is not disabled or bypassed. Signed source05015e3fa2f8e5436eb1af13f5f73310022f737a.
GNU and musl native corpora now each pass15/0/0 at these exact new core bytes,
including the same used-head witness; raw/linux-{gnu,musl}-context-fence retain
receipts, streams, diffs and exact source snapshots. The private remote checkout
remains HEAD1189 plus the checked single-file05015 core patch; both GPU source
hashes and exact product lock match. No wholesale sync or remote evidence
removal was used for this follow-up.

## Policy and remaining gates

R01: no new production allocation/unchecked indexing/arithmetic/poison path; the
private completion only moves the original Vec and adds an enum. Existing GPU
allocation risks remain unclosed. R02/R03: pinned primary sources and exact native
component red/green; no new production numeric literal. R04: typed request/signal
boundary, successful destroy event and preserved error/pause/resume transitions.
R05/R06/R07/R13: real worker caller, one existing drain owner, no polling/cache or
speculative interface; focused core change and non-obvious boundary comment.
R08/R14: all cost/profile/4–5x comparisons unrun, ROOT-owned. R09/R10: actual
native failure, source hashes/diffs/raw and explicit fixture failures retained.
R11/R12: shared platform-independent core; current Mac native component14/0/0 and new GNU/musl15/0/0 have
run at these exact new bytes; earlier passes are not inherited. R15: scoped
units and strict lint only; actual backend/VM/neighbor/malformed/fault/mutation and
old/new snapshot gates remain ROOT-owed. R16: private target only, no cache/evidence
cleanup, no bypass of admission or Mac headroom. Approved Hex ownership and
ROOT-owned workflow/release spans remain intact.

Current no lane build/test/API job is active. Final source core SHA256
68f22fdbce595b0b14624a40a5140754a05b4fbb18d43c2e000ee62357afa15e;
protocol SHA56bee5a5... unchanged. All137 payloads of the signed05015 source
checkpoint were independently checked against committed Git object lengths and
SHA256 before the Linux follow-up. The full staged evidence whitespace check
remains2 on authentic raw whitespace; final source-only diff/format/policy pass0.
