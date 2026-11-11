# Context destruction: callback source contract

Research only at Shards `6e34c5872d11b1c831038c13ebcdd36ccc2c338b`.
**There is no unconditional all-thread quiescence guarantee from the public void
destroy call.** Linux joins the client callback thread before returning, but does
not wait for the server's destroy reply. Krunkit's normal teardown joins its
context rings, monitor and queue workers before returning. These conclusions
assume successful joins, valid lifecycle state and serialized renderer entry;
the C callers discard join errors. No late-after-return callback was reproduced,
and no implementation or interface change is proposed by this record.

Implementer `01a1c76784eb72ccaf9096c88182efb7`; coordinator
`01a148884a4770fdaf0111f3f792a248`. POLICY_READ: current origin/main
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`, full R01–R16 SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Policy, evidence, mechanics, managed plan and current handoffs were read.
Owned changes in this slice are this evidence directory only.

## Exact selected inputs

The two official archives were downloaded, their complete bytes matched the
tracked `scripts/gpu-libs` pins, and regular source members were read without
executing or building them. [Source receipts](destroy-source-inputs.json) retain
every member length/SHA256. Original archives remain in the private
`target/managed-virtio-gpu-destroy-sources/` scope. Retained source includes its
upstream license notices and COPYING files.

| Tracked selection | Official source and full archive SHA256 |
| --- | --- |
| Linux GNU/musl, both architectures | [virglrenderer 1.3.0](https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/1.3.0/virglrenderer-1.3.0.tar.gz); `065bc56e89e6f631f96101cd62eba0748e48eb888b434edc86e89d05395e76f3` |
| macOS ARM64; Windows amd64/arm64 source selection | [krunkit 0.10.4e-krunkit](https://gitlab.freedesktop.org/slp/virglrenderer/-/archive/0.10.4e-krunkit/virglrenderer-0.10.4e-krunkit.tar.gz); `09d000623fbdb966cb604eb48c962a0815e8142383e6066d6494809335b76dbb` |
| Additional tracked Windows adaptation | `resources/gpu/venus-windows.patch`, SHA256 `9e500698a548cf396050c6a337ca0b3d66a054cf2602cc3f7bab925c21bb96bd`; this is a source pin, not a built/native Windows ARM renderer result |

Tracked `scripts/gpu-libs` SHA256 is
`f16feebce09c14568f4349f10d03e29fb982088115d864c28f67fe8b064ff10a`;
`gpu/virgl.rs` is
`7c6d189157fabed34b0043c9e9a667d66c451ec09aaa2f9cc9a9ca3189e3f34f`.
Linux requests VENUS, NO_VIRGL, RENDER_SERVER, THREAD_SYNC and ASYNC_FENCE_CB;
its selected build uses thread workers. Mac/Windows request VENUS and NO_VIRGL;
the fork's `venus_renderer_init` itself enables VKR THREAD_SYNC/ASYNC_FENCE_CB.
Their outer flag difference does not imply synchronous fence callbacks.

## Linux: client callback lifetime and server lifetime differ

1. `virgl_renderer_context_destroy` (virglrenderer.c:283–287) calls
   `virgl_context_remove` (virgl_context.c:81–84).
   [util_hash_table_remove](sources/destroy/linux/src/gallium/auxiliary/util/u_hash_table.c):136–154
   invokes `ht->destroy(item->data)` synchronously **before** removing the table
   entry. `virgl_context_destroy_func`:40–48 invokes the selected context's
   `destroy`, which Venus/proxy creation installed as `proxy_context_destroy`.
2. [proxy_context_destroy](sources/destroy/linux/src/proxy/proxy_context.c):505–545
   calls `proxy_client_destroy_context`, then sets `sync_thread.stop`, writes the
   eventfd and calls `thrd_join` at517 when that callback thread was created.
   Only afterward does it close/unmap/free the client context's owned state.
   [proxy_client_destroy_context](sources/destroy/linux/src/proxy/proxy_client.c):12–21
   only sends a request. It receives **no destroy acknowledgment**.
3. The client callback producer is `proxy_context_sync_thread`:165–194 →
   `proxy_context_retire_fences_internal`:141–162 →
   `proxy_context_retire_timeline_fences_locked`:104–138 →
   `base.fence_retire` → `per_context_fence_retire` → Shards'
   `write_context_fence`. The non-async retire entry explicitly asserts against
   this selected async mode. A thread already inside retirement can queue a
   callback while destruction is joining it, before the join/call returns.
4. Separately, [render_client_dispatch_destroy_context](sources/destroy/linux/server/render_client.c):173–182
   removes its record → `render_worker_destroy` → thread join
   ([render_worker.c](sources/destroy/linux/server/render_worker.c):407–424).
   Closing the client context socket lets the worker leave dispatch;
   [render_context_fini](sources/destroy/linux/server/render_context.c):251–268
   calls `render_state_destroy_context` → `vkr_renderer_destroy_context` →
   `vkr_context_destroy` to stop/join the server rings, monitor and queues.
   Server fence callbacks are `render_state_cb_retire_fence`:105–112 →
   `render_context_update_timeline`:16–24, storing shared seqnos/writing an
   eventfd. They do not call the Rust cookie directly. Client and server own
   their respective mappings/descriptors.

Thus successful client-thread join ends the selected **local old-context fence
callback producer**. It does not prove that all server/GPU work ended before the
client destroy returned. The separate server joins cannot be credited as an
acknowledged caller-return boundary. Per-context return also differs from
`Venus::drop`, which cleans up the renderer and joins the render-server thread.

## Krunkit: direct callbacks and synchronous retirement during teardown

The same synchronous hash-table removal invokes
[venus_context_destroy](sources/destroy/krunkit/src/venus_context.c):346–359 →
`vkr_renderer_destroy_context`:130–140 →
[vkr_context_destroy](sources/destroy/krunkit/src/venus/vkr_context.c):579–614.
The Venus wrapper remains in its separate lookup list until the inner destructor
finishes; `venus_remove_context` and wrapper free follow it. This matters because
`venus_state_cb_retire_fence`:422–429 looks up that wrapper before forwarding
through timeline retirement/base.fence_retire to Rust.

| Inner teardown, in order | Exact source ownership and return boundary |
| --- | --- |
| Stop each command ring | [vkr_ring_stop](sources/destroy/krunkit/src/venus/vkr_ring.c):320–335 clears started, signals, then joins; `vkr_ring_destroy`:160–169 asserts stopped and frees it. Self-stop returns false; the context destructor ignores that outcome. Shards calls destruction from its device worker, not from a renderer ring/callback thread. |
| Stop monitor if started | `vkr_context_ring_monitor_fini`:564–576 clears started under its mutex, signals and joins before destroying monitor synchronization. |
| Destroy instance and its physical devices/devices | `vkr_instance_destroy`:227–248 → `vkr_physical_device_destroy`:57–67 → [vkr_device_destroy](sources/destroy/krunkit/src/venus/vkr_device.c):250–285 calls DeviceWaitIdle, logs errors, destroys tracked objects/queues, then Vulkan device. |
| Finish each queue | [vkr_queue_sync_thread_fini](sources/destroy/krunkit/src/venus/vkr_queue.c):113–129 sets join under its mutex, signals, joins, **then calls vkr_queue_sync_retire on pending syncs synchronously** before freeing synchronization/queue. |

Fence callback origins in this selected direct backend are the synchronous ring0
path (`vkr_context_submit_fence`:123–145) and queue retirement
(`vkr_queue_sync_retire`:71–75), called by the queue worker or the destruction
thread's pending-sync loop. No direct old-context fence producer is intentionally
left running on successful joins. The remaining pending-sync loop specifically
permits callbacks **during destroy, before return**; it does not establish that
every fence represented successful GPU execution.

The tracked Windows patch changes ring self-identification to
`GetThreadId(handle) == GetCurrentThreadId()` (patch:896–910), retaining the same
stop/join sequence. The pinned C11 wrapper maps join to `pthread_join` on POSIX
([threads_posix.h](sources/destroy/krunkit/src/mesa/compat/c11/threads_posix.h):326–334)
or `WaitForSingleObject(INFINITE)` followed by CloseHandle on Windows
([threads_win32.h](sources/destroy/krunkit/src/mesa/compat/c11/threads_win32.h):438–453).
Each reports errors; renderer join call sites discard them. Microsoft's
[wait contract](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject)
also distinguishes a signaled thread from wait failure. No bounded join/driver
liveness, native Windows ARM behavior, or termination of every Vulkan loader/
driver-global thread is established here.

## Existing Renderer contract and the component proof

The existing API remains `destroy_context(&mut self, ctx: u32)` followed by
`signals(&mut Vec<Signal>)`; no signature, generation map, purge or new holder.
Renderer entry is serialized by the GPU worker. `write_context_fence`
(virgl.rs:590–608) queues in Shared and unparks that worker. `destroy_context`
(1049–1053) calls C then removes the ID from its owned list; it does not empty
Shared.signals. `signals` (1127–1133) drains those queued events.

The source-supported normal-path requirement is: old-context producers have
stopped when destroy returns, but callbacks already queued before/during the
call may remain. **This is conditional on successful native teardown, which the
void FFI does not report.** It is not a newly verified backend guarantee.

Keep [GPU-CTX-1](CONTEXT-FENCES.md) unchanged: baseline native component actual
used index5 instead of4; signed `05015e3fa2f8e5436eb1af13f5f73310022f737a`
inserts the More/drain boundary before same-ID reuse. Final strengthened fixture
observed heads `[0,2,4,6]`, then `[0,2,4,6,8]` only after the fresh signal.
Mac14/0/0 and GNU/musl15/0/0 are component results. They prove the
**queued-before-return** case. A callback first arriving **after return and after
that drain** is a distinct case; this boundary alone does not prove safety for
it, and this slice neither reproduces nor fixes it.

## Established implementations and limits

[QEMU v10.1.0](https://github.com/qemu/qemu/blob/v10.1.0/hw/display/virtio-gpu-virgl.c)
directly calls the same void C destroy (360–369). Its context fence callback
(1015–1034) updates the device fence queue directly. Its selected Venus flags
(1155–1158) omit Shards' explicit async flag, and fence polling calls
`virgl_renderer_poll` (1106–1115). That different ownership/thread model is not
proof of Shards' async boundary. [libkrun at1f5dd0288fdd3c2da5eb5b555126f82f2efc53d7](https://github.com/containers/libkrun/blob/1f5dd0288fdd3c2da5eb5b555126f82f2efc53d7/src/devices/src/virtio/gpu/virtio_gpu.rs)
delegates destruction to Rutabaga (703–705); its fence handler captures queue and
fence state Arcs and locks them while publishing completions (198–251). That
wrapper does not establish the selected C backend's join semantics either.
Full retrieved bytes/hashes are in [comparison receipts](destroy-comparison-inputs.json).
These are established alternatives, not new runtime comparison results.
The abbreviated libkrun revision was resolved through its official commit API;
[resolution receipt](destroy-libkrun-pin.json) and raw response are retained.

ROOT retains the actual selected/patched-backend late-callback, GPU VM, old/new
snapshot/restore, Linux/Windows ARM hardware, combined seven-target and cost
gates. Hex L2 was sent exact source-chain/contract findings (seq7354,7427); its
physical-selector/filter/ACK branch owns virgl.rs and packaging. This record
does not certify unreceived new renderer patches or change that interface.
Protocol/fence branches remain pending ROOT combined review, not main-ready.

### Supplied selector patches: exact source follow-up

Hex L2 subsequently supplied `de0a0e6eca64539910da46fd95ba8ff8c7b7fa81` in
its separate `shards-physical-gpu` checkout. All four patch Git blobs were read
there without modifying/adopting that checkout, and matched both the supplied
hashes and the exact historical Hex `6219e61c9e3f559579b2632f873005a47f521cc8`
Git blobs. [Receipts and retained copies](destroy-selected-patch-inputs.json):

| Patch | Full SHA256 |
| --- | --- |
| physical-linux | `92163c9882304fa0a446d230fb80b4fb8399aad8c39eb7811e5d3937b9de0512` |
| physical-krunkit | `03c7cb34a2f39622cd5794f6a867bc5ba387aad82ce04f12a8dbb3374ed283ef` |
| physical-transport-linux | `63d805201465ec32559287784a489f4a3fd3004a18f51d602b3be43402078dff` |
| physical-transport-krunkit | `2025d8c3285806c4555d1f0f80935ce0b86dd073424c292613ab6f51821c40c1` |

Both physical patches add `vkr_physical_device_filter_finish` immediately before
debug-name/context free, after the existing ring/monitor/instance/queue teardown
(each patch:4–11). They do not change stop/join/fence-retirement algorithms.
The transport patches add a SET_UUIDS reply and selection-failure socket shutdown
(Linux patch:8–62,244–310), not a context-destruction acknowledgment. Socket FD
ownership remains with ordinary destruction; an invalid selection can end the
server's existing dispatch loop earlier. The server cleanup chain above remains
separate from the client destroy return.

The pinned Rust adapter still queues callbacks, destroys context through the
same void C call and drains Shared.signals (virgl.rs:729–747,1222–1226,1300–1302).
Its new selection-failure branch destroys a newly created context before
publishing it in the context list or allowing guest commands (1185–1218).
The build script still pins the same archives, applying the tracked Windows
patch before physical/transport patches; that Windows patch is byte-equal to the
tracked `9e500698...` input. Therefore these supplied source deltas do not add
the missing unconditional/late-after-return guarantee. No patches were applied
or renderer code run here. This is a destructor-contract comparison, not broad
selector, whitelist, build, ABI, VM-helper or native compatibility acceptance.
The peer's API wait_for failure and VM-helper physical proof remain unresolved.
ROOT retains those gates and combined review. Coordination sent back to Hex L2
(seq7656); no conflicting Renderer interface or fix is introduced.

## Validation and policy scope

Run `python3 docs/research/measurements/managed-virtio-gpu/verify-destroy-sources.py`
for retained-source/archive checks, and `scripts/check-development-policy` for
the mechanical policy check. Results/attempts are retained under raw/destroy-*
and destroy-validation.json. No code was changed or native callback/fault/ENOMEM/
VM/profile/benchmark job run for this slice; no test rerun is claimed.
Source verification76 payloads/two archives and policy both returned0. Authored
staged note/handoff/helper whitespace check returned0; full evidence check
returned2 on unchanged upstream source/license whitespace, retained verbatim.

R01/R03/R06/R07/R13: documentation/primary sources only; no production panic,
literal, complexity or comment change. R02: exact source/alternative contracts
above; missing native data stays unverified. R04: caller/producer/queue/return
transitions distinguished. R05: existing join/readiness waits examined; no wait,
polling or cache mechanism adopted. R08/R14: all performance comparisons unrun,
no win claimed. R09/R10: no new defect/reproduction claim; prior exact red/green
and every research failure retained. R11/R12: platform source selection above,
no new portability/native pass or duplicate implementation. R15: finite research
only, no children or campaigns; ROOT review/gates remain. R16: sources/artifacts
preserved, no cache cleanup, host change or admission bypass. No active lane
build/test/API job remains.
