# Context fence lifecycle: source-only follow-up

No defect is claimed reproduced and no production fix is made here. Pinned
source receipts are in context-source-receipts.json. Reading these files is
research under the existing GPU scope, not a renderer or fault campaign.

Potential sequence to investigate: a renderer callback queues a completed fence;
the device processes context destruction and creation of the same numeric ID
before its next signal drain; a fresh per-context fence sequence could match the
old queued signal's context/ring/number. Served releases waiting requests on
destroy, while Venus::Shared keeps queued signals until the worker drains them.
Whether an actual supported guest/renderer sequence exhibits an incorrect
completion remains unverified. The ordinary Linux driver's global monotonic
fence IDs may make this specific sequence unlike its usual traffic. Do not label
it a demonstrated race, exploit or old/new snapshot failure.

Primary observations: virglrenderer1.3.0 proxy_context_destroy joins its sync
thread before freeing the context; the krunkit fork's vkr_context_destroy stops
rings and destroys its Vulkan instance. These support investigation of a possible
end-of-destroy drain, but do not establish every callback producer's termination
without reviewing the remaining instance/device/sync teardown. Its async fence
queue and allocation-failure behavior also remain to be tested at actual callers.

Alternatives requiring evaluation: retire queued signals at the renderer's
existing destroy boundary, drain at the device boundary while preserving other
contexts' completions, or add a context generation to the existing callback
identity. The last expands the interface and state; no speculative generation
manager was introduced to avoid gathering the first two proofs. No waits,
polling, cache, fence limit or timing constant was added.

Ownership: virgl.rs is untouched while Shards ROOT and Hex ROOT resolve the
physical-selector lane partition. Any overlapping context/destroy/renderer ABI
change must be coordinated by exact span first. ROOT owns actual GPU/VM,
mutation/fault and timing/resource validation. A future native fake-renderer
state unit would only prove that bounded component sequence, not host GPU
behavior, and is not reported as executed here.
