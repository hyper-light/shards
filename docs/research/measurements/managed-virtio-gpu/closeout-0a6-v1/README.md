# Owned virtio-GPU code closeout

The complete owned code delta is [owned-code.patch](owned-code.patch): **15,384
bytes**, SHA256 `e506d40cb39506518178992091333a8554e9c30238ae7c088b68ab557131e1dd`.
It applies to current main `0a6d91b3de0f10a8397868aa3794adab1e09afdc` and changes
only `crates/vmm/src/devices/virtio/gpu/{mod.rs,protocol.rs}`, including native
regressions. It contains no documentation, selector, renderer, VM configuration,
ABI or packaging changes. The owned resource/window implementation already has
runtime callers; `window.rs` and `render_log.c` are unchanged, explicitly hashed
in [source-manifest.json](source-manifest.json). No new unused API is being
offered as completion.

These are the product bytes of signed protocol commit
`118978b03c0f27f17a995bcecb1d7be3dedb3221` and core commit
`05015e3fa2f8e5436eb1af13f5f73310022f737a`, retained by signed lane checkpoint
`5b13458e19b32f233792ea5fe6e1a754243c6c89`. The combined code-only patch is a
frozen unsigned selection artifact for ROOT's signer, not a new signed commit.
The current shell has no own signer; no socket/key was borrowed or signing
disabled. ROOT owns evidence review, combined validation and main integration.

POLICY_READ: full current main0a6 AGENTS/R01–R16/evidence/CLAUDE/managed plan and
latest `/Users/adalundhe/g121-handoff.md` read; rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Implementer `01a1c76784eb72ccaf9096c88182efb7`; ROOT
`01a148884a4770fdaf0111f3f792a248`. No rebase, main merge, new VM, renderer
execution, driver/admin operation, campaign, profile or benchmark occurred.

## Changes and actual callers

1. **Empty descriptors and response scattering.** `protocol::read`:194–208
   continues past empty readable descriptors. `protocol::answer`:276–319 moves
   checked borrowed header/field slices across writable descriptors, including
   empty ones, with checked address/used-length accumulation and prefix-only
   return on an inaccessible/short destination. This removes the former
   per-descriptor temporary response Vec in source; no measured cost win is
   claimed. Original native red:3 passed/2 failed. Later combined green retains
   split header/fields, fence echo, empty inaccessible descriptor addresses and
   no writes after an inaccessible nonempty range.
2. **Queued old completion before context-ID reuse.** Successful CtxDestroy
   returns private `Completion { used, next: Signals }`; the real `round` yields
   More and runs its existing signal drain before the next request. Typed invalid
   destruction keeps Request. No Renderer signature, generation map, callback
   purge, polling, additional manager, production literal or production
   allocation was added. Original native component red used index5 instead of4.

The production path is already wired: both architecture VM constructors
(`vm/x86_64.rs`:488–491, `vm/aarch64.rs`:625–628) call
`vm::gpu_device`:976–987 → `Served::new` → `Gpu::new` → VirtioDevice activation
→ `Running::start`:657–697 → `round`:591–640 → `Served::serve`:257–315 →
`protocol::{read,answer}` and `Served::command`. Context commands invoke the
existing Renderer; blob creation/map/unmap/unref/reset invoke its resource
methods and Window (`mod.rs`:383–585). Linux's existing `gpu_window` calls
`Reserved::new`/map/unmap (`vm/mod.rs`:1002–1015, `window.rs`:30–80); HVF/WHP
use the existing platform windows (`vm/mod.rs`:991–1054). These caller and build
inputs are byte-equal between base0a6 and the tested lane, as recorded in the
manifest. They are source wiring evidence, not native GPU VM results.

No frozen constructor/type edit is required from L2. Exact shared tokens remain:
`Served::new(Box<dyn Renderer>, Box<dyn Window>, ShmRegion, u64, usize) -> Served`,
`Gpu::new(Box<Served>) -> Gpu`, and the existing Renderer create/destroy/fence/
signals methods. `serve`/Completion/Next remain private. L2 retains identity,
selection, positive ACK, virgl.rs, VM-config, snapshot-selector and packaging
ownership. Last consumed immutable L2 source is de0a0e6e; its exact supplied
patches and matching contract are in [the destructor note](../DESTROY-CONTRACT.md).
No active L2 writer files were read/copied for this closeout. A newer frozen L2
composition revision is ROOT's input, not an assumed inherited result.

## Native results at the exact supplied code bytes

[native-evidence.json](native-evidence.json) identifies commands, platforms,
actual working-source hashes, receipt heads, results and42 retained raw payload
hashes for8 selected records. A receipt's old HEAD alone is not treated as the
source identity: final GPU file hashes match the supplied source manifest.

| Native target | Actual final core/protocol unit result | Strict final VMM lib/tests Clippy | Renderer/GPU VM/restore |
| --- | --- | --- | --- |
| aarch64-apple-darwin |14 passed/0 failed/0 ignored |0 |Unrun |
| x86_64-unknown-linux-gnu |15/0/0, includes real reserved-window mapping unit |Unrun at final core bytes |Unrun |
| x86_64-unknown-linux-musl |15/0/0, native execution of musl-target binary, same window unit |Unrun at final core bytes |Unrun |
| aarch64-unknown-linux-gnu |Unrun |Unrun |Unrun |
| aarch64-unknown-linux-musl |Unrun |Unrun |Unrun |
| x86_64-pc-windows-msvc |Unrun |Unrun |Unrun |
| aarch64-pc-windows-msvc |Unrun; paused Windows ARM VM untouched |Unrun |Unrun |

All recorded native executions used the original private2CPU/2GiB/4GiB/600s/-j2
envelope, exact product Cargo.lock SHA256
`1ecb41e738c4c4133a1968873e424efce5b2a65899dd8f40f37ac6aaffec8133`;
Mac lockf and30GiB floor were retained. These are core tests with the existing
test Renderer, not feature-gpu native backend tests. No new native run was
needed to repackage unchanged code; none is reported. The previous readonly
Metal/Vulkan enumerations remain host metadata only. ROOT's main/capture gates
do not substitute for the patched combined GPU gate.

The strengthened queued-case fixture publishes one batch while the worker is
paused. Its old fenced request is pending when destroy begins; the existing
test Renderer queues a completion during destruction **before it returns**.
Final actual used heads are `[0,2,4,6]`; only a fresh signal releases head8,
producing `[0,2,4,6,8]`. A first callback arriving **after destroy returns and
after the drain** is distinct and unverified. It was not reproduced or fixed.
The selected C source has conditional join paths, ignored join errors and, on
Linux, no destroy ACK; [DESTROY-CONTRACT.md](../DESTROY-CONTRACT.md) states the
precise limits. No unconditional backend quiescence or physical-GPU claim.

Original3/2 protocol red,0/1 context red, the later13/1 Reentered fixture-helper
failure and its corrected14/0 green remain retained. The Clippy admission
connection refusal preceded a later normally admitted strict pass; it was not
bypassed. Existing raw logs/traces, source snapshots, failed attempts and
authentic whitespace are preserved. See [README](../README.md),
[CONTEXT-FENCES](../CONTEXT-FENCES.md) and [NATIVE](../NATIVE.md).

## Composition checks and external inputs

[source-application-check.json](source-application-check.json) records an isolated
private-index read-tree of main0a6, code-patch check/apply0, and exact resulting
hashes for all four owned sources. It did not modify a worktree, branch or main.
The manifest preserves all current capture changes by selecting only two GPU
files. Mechanical policy/format/source checks are recorded in validation.json;
they are not combined integration proof.

Concrete remaining ROOT inputs/gates:

- Review/select this exact two-file patch with the final frozen L2 selector,
  positive-ACK, schema, build/renderer-source and immutable-snapshot selection
  revision; run the seven-target/GPU feature gates on that combined revision.
- Actual authorized GPU hosts and confined renderer binaries with device/driver/
  API/source identities, including required Linux/Windows ARM hosts and the
  eventual approved NVIDIA host. Never substitute software or another adapter
  as selected-device parity; paused Windows ARM remains paused.
- Actual backend callback lifecycle evidence separating pending/queued-before-
  return from first-late-after-return, valid context/resource/fence/reset cleanup,
  and actual GPU guest/driver commands. No source-risk-only callback fix is owed
  without a valid live reproduction.
- Old/new snapshot fixtures and the final selector policy, including supported
  pre-resource templates and checked refusal of unsaved live GPU state. Existing
  snapshot refusal is not renderer-state restoration proof.
- Resource/failure/largest-boundary/neighbor/control proofs and parent/kernel
  accounting; meaningful mutations and allocation-failure gates are ROOT-owned.
  Existing infallible allocation paths are not certified panic-free by these
  successful units. The held allocator/callback campaigns remain unrun here.
- Equivalent pinned workload/baseline and actual profile/benchmark distributions
  for CPU, allocation/copy/fault, retained memory and kernel costs. No4–5x or
  whole-path allocation/leak-free claim is made.

No missing owned runtime caller or constructor input prevents selecting this
implemented two-defect piece. The remaining gates require actual ROOT integration
and backend evidence; this closeout does not claim full GPU parity or P0–P13.

R01/R04: new checked descriptor operations/private completion transition and
preserved typed outcomes; existing allocation/failure uncertainties stay open.
R02/R03: pinned spec/guest/renderer alternatives and exact live component reds
are retained; no new production literal. R05/R06/R07/R13: actual existing worker
and one authoritative drain; no new framework/cache/polling. R08/R14: costs
unrun. R09/R10: exact red/green/source/raw and failures retained; mutation and
backend evidence not fabricated. R11/R12: shared core, explicit seven-target
matrix, existing platform seams. R15: scoped light results only, no children,
main merge or new held effect. R16: no source/evidence/cache cleanup, headroom
or admission bypass. No lane build/test/API job is active.
