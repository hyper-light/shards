# Managed virtio-gpu compatibility lane

Status: first scoped protocol fix; GPU parity and the management layer remain
incomplete. Coordinator: `01a148884a4770fdaf0111f3f792a248`. Implementer:
`01a1c76784eb72ccaf9096c88182efb7`. Source base:
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`, branch
`p/managed-virtio-gpu-kiln-v1`.

## Policy and ownership

POLICY_READ: current `origin/main:AGENTS.md`, full
`docs/development-rules.md` R01–R16, `docs/development-evidence.md`, `CLAUDE.md`,
full `docs/design/managed-runtime-plan.md`, and `/Users/adalundhe/g121-handoff.md`
read before edits/builds/tests. Rules SHA-256:
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
`git ls-remote origin refs/heads/main` matched the base. ROOT receipt seq6941;
first interfaces/state/alternatives seq6976; reproduction and build-span proposal
seq7029. A receipt identifies loaded instructions; it is not compliance proof.

Owned production paths: `crates/vmm/src/devices/virtio/gpu/{mod.rs,protocol.rs,
virgl.rs,window.rs,render_log.c}` and demonstrated GPU-only tests/adapters.
Owned evidence: this directory. This first patch changes only `protocol.rs`.
Shared queue/ABI/kernel/hv/PCI/IOMMU/startup/build and workflow changes require
ROOT's exact-span agreement. No children, goal/plan wrappers or main merges.
Paused Windows ARM VM, held campaigns and other lanes' caches remain untouched.

## Existing behavior and real callers

The device already implements headless Vulkan through Venus, capset 4. It offers
VIRGL (the Linux 3D ioctl gate), RESOURCE_BLOB, CONTEXT_INIT and BLOB_ALIGNMENT,
zero scanouts, two queues and a host-visible shared-memory window. It implements
capset queries, context creation/destruction, blob creation/unref/map/unmap,
context resource attachment/detachment and 3D submission. Traditional 2D/virgl GL
resource/transfer commands are unsupported; they are not advertised as GL parity.
`AttachBacking`/`DetachBacking` parse but return ERR_UNSPEC. Whether a supported
Venus use requires these backing transitions remains unverified.

Real path: `Gpu::activate -> Running::start -> round -> Served::serve ->
protocol::{read,answer}`. This is the existing production dispatch path, not a
new manager or unused parser. `Served` owns context IDs, resource IDs, window
ranges, held request heads and per-context/ring fence queues. A fenced reply
waits for its signal and earlier replies on the same timeline. Destroy releases
that context's waiting requests. Reset removes mappings, renderer resources,
contexts and pending replies. These existing transitions are exercised by the
scoped unit corpus, not newly certified against hostile GPU workloads here.

Snapshots remain limited to an empty renderer: live contexts/resources/fences
refuse a save. D150's restored templates recreate the renderer before any guest
GPU objects exist. No live GPU-state migration is implemented by this patch.

The one installed binary still carries its GPU VM helper. GNU and musl GPU
helpers dynamically load drivers of the corresponding host libc; other musl
helpers remain static. macOS ARM64 uses the krunkit fork with static MoltenVK;
Linux uses virglrenderer 1.3.0 plus its in-process threaded render server; Windows
amd64 uses the patched krunkit fork and the host Vulkan loader. Linux blob pages
are held by exported descriptors, Mac pages by a shared Mach remap, Windows pages
by the renderer's imported host allocation. No fallback/driver/capability change.

## Pinned research and D-GPU-SG-1

[Source receipts](sources.json) retain exact URLs, content lengths and SHA-256.
Code/header sources are retained under `sources/` with their license headers.
Read the existing GPU research plus D144/D150/D152/D201/D202/D208 and
M214/M216/M218/M223/M381–386/M425; their historical measurements are not new
revision gates.

Primary sources compared for the narrow scatter/gather decision:

- [VIRTIO 1.3 CSD01](https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html),
  section 2.7.4 and GPU control messages in section 5.7.6: message layout cannot
  assume a particular descriptor arrangement. Empty segments contribute no bytes.
- [QEMU v10.1.0 GPU response](https://github.com/qemu/qemu/blob/v10.1.0/hw/display/virtio-gpu.c#L146)
  uses `iov_from_buf`; its
  [iov implementation](https://github.com/qemu/qemu/blob/v10.1.0/util/iov.c#L25)
  skips empty segments and copies spans directly. This is an established shared
  scatter/gather alternative, not a source for an unexplained runtime limit.
- [Linux v6.18 guest queue](https://github.com/torvalds/linux/blob/v6.18/drivers/gpu/drm/virtio/virtgpu_vq.c#L425)
  submits command/data/response scatterlists. Its usual response is one segment;
  that common shape does not justify assuming every valid chain has that shape.
  [UAPI](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/virtio_gpu.h)
  defines the 24-byte header, fences and protocol field layouts.
- [Mesa 25.0.7 Venus](https://gitlab.freedesktop.org/mesa/mesa/-/blob/mesa-25.0.7/docs/drivers/venus.rst)
  and the current official Venus documentation distinguish Vulkan, blob memory,
  driver and kernel requirements. They do not establish arbitrary host-driver
  parity. Pinned virglrenderer 1.3.0 and krunkit 0.10.4e headers confirm that the
  existing renderer seam serves Venus without another renderer framework.

Alternatives: (a) only change the reply's empty segment from `break` to `continue`,
keeping a temporary Vec for every nonempty reply descriptor; (b) scatter borrowed
header/field slices through checked guest-memory writes. Choose (b), following
the existing Access API and established iovec traversal, after the exact red
below. It also removes the temporary reply Vec and its host-to-host copy by
construction. No allocation count, latency, fault or resource performance win
has been measured; ROOT owns those measurements. A generic new iovec layer is
unnecessary for this two-part reply and would enlarge the shared seam.

Meaningful state is the remaining source slice and the current descriptor's
address/room. Empty segments advance only the descriptor; exhausted source
advances to the next part; successful writes advance source, address and used
length; an inaccessible range, address overflow or unrepresentable used count
terminates at the successfully written prefix. A short chain returns its actual
prefix. No later buffer is written after a failed write. No new production
constant, cache, retry deadline, poll or authority is introduced. The existing
read-request limit still bounds the copied request; the empty-segment fix does
not enlarge it.

## Exact native reproduction and validation

`light-check.py` rejects an existing result directory and a modified product
Cargo.lock, removes DYLD overrides, sets one private target and cargo -j2, and
retains command, source hashes, source diff, protocol source and raw streams.
All native checks use Kiln admission 2CPU/2GiB/4GiB/600s, a 570s child timeout,
Mac `lockf -t 0 /tmp/g121-mac.lock`, and the 30GiB free-space floor. These are
user-specified test-envelope inputs, not new product policy constants. Runner:
`e30189ea29182644fe5a4b2e06c7dac2`, macOS 26.4.1 ARM64. Product lock SHA-256:
`1ecb41e738c4c4133a1968873e424efce5b2a65899dd8f40f37ac6aaffec8133`.

Repeat from the repository root under the same admission and lock, with a fresh
label:

```sh
lockf -t 0 /tmp/g121-mac.lock python3 docs/research/measurements/managed-virtio-gpu/light-check.py fresh-label cargo test --locked -j2 -p shards-vmm --lib devices::virtio::gpu:: -- --nocapture --test-threads=1
```

| Raw directory | Actual result |
| --- | --- |
| `raw/red-empty-descriptors` | Baseline production plus two new regressions: exit101, 3 existing tests pass, 2 fail, 0 ignored. The valid 28-byte capset request behind an empty segment returns None; a fenced 32-byte reply writes0. Exact GuestMemory/Chain native calls; no fake renderer, VM or hardware inference. |
| `raw/green-empty-descriptors` | First production correction: exit0, 12 GPU unit tests pass, 0 ignored, including existing worker/fence tests. |
| `raw/clippy-first` | First corrected source, strict VMM lib/tests Clippy exit0. |
| `raw/green-boundaries` | Final source including short/inaccessible reply-prefix check: exit0, 13 GPU unit tests pass, 0 ignored. |
| `raw/clippy-final-framing` | Final source, strict VMM lib/tests Clippy exit0. |

The regressions use empty descriptors at the front, middle and tail, with
inaccessible addresses proving they are not touched; a response descriptor spans
the header/field boundary and verifies echoed context/ring/fence. Short replies
preserve the unwritten sentinel; a nonempty inaccessible descriptor returns the
written prefix and leaves later memory unchanged. Existing successful tests and
assertions remain present. Baseline red is a real protocol symptom, not a source
risk or GPU-host reproduction. Guard-removal campaigns, native renderer linking,
actual GPU VMs and old/new snapshot compatibility are ROOT-owed, unrun here.

## Compatibility matrix at this checkpoint

Historical results below are linked repository evidence, not independently
rerun current-source passes. No unavailable host is counted as passed.

| Host | Existing source/package | Historical GPU-host evidence | Current lane evidence / remaining gap |
| --- | --- | --- | --- |
| macOS ARM64 | Shared device; krunkit/MoltenVK; carried helper | D144/D150, M216/M218/M223 | Native protocol/worker corpus above; actual current GPU VM and native renderer unrun |
| Linux GNU amd64 | virglrenderer1.3.0 threaded server, Vulkan, descriptor window | D201/D208; NVIDIA/RADV/ANV/NVK | Current native/renderer/VM not yet run by this lane |
| Linux musl amd64 | Dynamic musl GPU helper, static outer binary | D208/M425; Alpine/ANV | Current native/renderer/VM not yet run by this lane |
| Linux GNU/musl ARM64 | Existing shared implementation/build targets | D201 explicitly leaves actual GPU host untested | Unverified actual GPU host, PFN mapping/cache behavior and current revision |
| Windows amd64 | Patched krunkit fork; host-imported pages, private desktop | D202/M383/M385/M386; AMD8060S | Current native renderer/VM and Intel/NVIDIA hosts unrun |
| Windows ARM64 | Shared Rust device builds; GPU helper/package target excluded | None | **Missing GPU packaging**, not a passing skip; exact GPU-specific build spans proposed to ROOT before edits |

The Windows ARM64 omission is at `crates/shards/build.rs::gpu_target`, the
Windows-x86_64 paths in `scripts/gpu-libs`, and GPU-only CI/release conditions.
Those shared/build paths remain held pending ROOT's agreement and concrete
crossbuild/toolchain research. The paused Windows ARM VM stays paused. No Intel
Mac support. NVIDIA adacachy2 remains unavailable according to the task; no
installation/root/control workaround is authorized.

## R01–R16 evidence and coordinator work owed

| Rule | Scoped evidence and limit |
| --- | --- |
| R01 | Reply traversal has no temporary allocation, checked source split/address/used count; room subtraction is bounded by min(room,part.len). No unwrap/index/assert added to production. Existing request Vec allocations and other GPU allocation paths remain an unclosed broader risk; this is not a whole-device panic-free proof. Strict Clippy above. |
| R02 | Pinned spec, Linux guest, QEMU, Mesa and renderer sources/hashes; real native red/green. Broader renderer/ABI/packaging decisions remain held for primary research and actual data. |
| R03 | No new production literals or arbitrary limits; header size is the existing wire contract. Test envelope and explicit fixture bytes are documented above. |
| R04 | Explicit source/descriptor transitions, checked terminal outcomes and unchanged typed Refused protocol responses. Existing context/resource/fence lifecycle tests retained. |
| R05 | Reuses protocol/Access seam with real worker callers. No cache/new wait/poll/framework. Existing fence callbacks unpark the worker; worker park/notification behavior unchanged. No performance claim from source inspection. |
| R06 | Focused read skip plus two-part reply traversal; no cross-platform duplication. Existing GPU lifecycle complexity not certified by this patch. |
| R07 | Comments retained only for layout/safety and the inaccessible empty descriptor proof; no copied source narration added to production. |
| R08 | No benchmark or4–5x claim. ROOT owes pinned equivalent GPU/VMM workload comparisons and misses. |
| R09 | Exact baseline None/zero-byte failures before production edit, retained raw/native command and source. No unproved lifetime/driver fix bundled. |
| R10 | This decision, raw streams, reproducible harness, source snapshots/hashes; ROOT owes meaningful guard-removal/forbidden-effect and actual GPU-boundary evidence. |
| R11 | Seven supported targets retained; matrix explicitly distinguishes historical/source/current/unverified. No Intel Mac or paused Windows ARM VM run. |
| R12 | One shared protocol implementation, existing GPU renderer/window/platform seams and carried-helper packaging. Shared changes held. |
| R13 | Small complete empty-descriptor behavior reaches real callers and preserves short/error behavior; no unused new enum/interface. |
| R14 | No profiling/benchmark performed. ROOT owes allocations/copies/faults/parent+kernel/GPU resources, distributions and contention. |
| R15 | Scoped native tests only, signed checkpoint returned for ROOT review. ROOT owns audit, combined gates, GPUVM/fault/chaos/mutation/restore integration and main merges. |
| R16 | Private target621MiB at first check, free storage >172GiB; no cleanup/deletion of other caches, source, credentials or evidence. |

Final policy check and checked rustfmt exit0. Production-path diff check exit0.
Complete staged diff check exit2 reports only authentic retained raw diff context
spaces and unit stdout EOF blank lines; `staged-diff-check.stdout` retains that
result. Evidence bytes were not edited to make a whitespace gate pass. Source
check is narrower and is not reported as a complete staged whitespace pass.
