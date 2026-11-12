# Hex's GPU patch 0006: a reused context id takes nothing of the destroyed one's

**What it pins.** A guest numbers fences with one counter per device (Linux virtgpu_fence.c,
`++drv->current_fence_id`) and may reuse a context id once the context is destroyed. The test
(Hex's 0006, shards commit cb75374d on Hex's hex/physical-gpu-0342) has context 1 wait on fence
10, destroys it (its waiting answer let go with the destroy's own), makes context 1 again and has
it wait on fence 11; a late signal of the destroyed incarnation's fence 10 then lets go of
nothing, and fence 11's lets go of the new answer. It tests `Served` directly, beside the
integration's `a_recreated_context_waits_for_its_own_fence` (d4b922b2), which tests the device's
worker draining old callbacks before a reused id's requests. Test-only: no production change.

**Green and guard removal.** On adacachy (451397fc), 2026-11-11, base 03786682 with the test:
`cargo test -p shards-vmm --lib devices::virtio::gpu::` 16 passed, the new test among them.
`mutations/g0006-destroy-keeps-waiting.patch` (CtxDestroy keeps the destroyed context's waiting
answers instead of letting them go) fails it at mod.rs:1441 (the destroy's answers), with
`contexts_are_of_the_capsets_offered` and `a_recreated_context_waits_for_its_own_fence`: 13
passed, 3 failed (`mutation.sh g0006-mutation`).
