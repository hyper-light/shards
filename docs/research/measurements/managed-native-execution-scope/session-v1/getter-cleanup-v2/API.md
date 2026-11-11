# Actual consumer getter cleanup

This narrow delta follows the actual init runtime consumer. It supersedes only
the getter portion of the earlier API.md; acquisition, publication, cleanup,
identity storage, syscalls and the shared parent delta are unchanged.

Before: commit `25d9da41ffe580879a7d8d6753d25dc14944450a`, leaf SHA-256
`f1dc920bf54cc1d2bb86025f77f5658b9485f83bd26b87febf7a418df80405cb`.
After: `execution.rs`, SHA-256
`7878ad24229335ce3f2fa6ef9c06cd1e3f03d8837d62a56dcb1cefec833424e3`.
`UNUSED-GETTERS.patch` is the exact tiny delta against that f1 source.

Removed five methods:

- `Prepared::job`
- `Prepared::directory`
- `Published::job`
- `Retained::job`
- `Retained::directory`

The actual runtime owner already owns the original checked Job ID and publishes
before borrowing only `Published::directory`. No fake use, lint suppression,
replacement getter, new constructor or runtime branch is introduced. Tests
inspect their child module's actual private fields/Holding variants directly.
The same internal job fields remain for existing cleanup/retention transitions.

The resulting callable API is:

```text
Prepared::acquire(&File actual_held_member, NonZeroU64 checked_Worker)
    -> Acquisition::{Prepared, Refused, Retained}
Prepared::publish() -> (Published, managed::Controls)
Prepared::cancel_before_birth() -> Cleanup::{Removed, Retained}
Published::directory() -> &File
Published::remove_after_reap() -> Cleanup
Retained::preparation_failure() -> Option<Refusal>
Retained::removal_failure() -> Rollback
Retained::retry_remove() -> Cleanup
```

Publication still precedes any Group FD handoff or possible birth. Actual
matching reap plus kernel populated0 remain the owner's prerequisites before
post-birth removal. Unknown keeps the same held objects and charges. This API
cleanup changes none of those obligations and proves no kernel behavior.

Actual runtime v1 readback retained in `actual-consumer-before/` reports three
unused-method warning groups naming those five methods. Its overall init
compile failed 101 with other independent caller/type errors too. Tests/Clippy
did not run in that actual graph, and this scoped edit does not claim to fix all
of it. `BASELINE-CONSUMER.json` identifies the exact raw archive/command/status/
stderr/source receipt. That old actual runtime used early leaf `9119e887...`, so
the runtime must adopt the exact updated final leaf and existing parent v2
dependency rather than blindly apply an f1-context patch to that older leaf.

Shared dependency remains the prior unapplied parent v2 patch SHA-256
`9b668c542cb6c74d7d06a33af0f32aa6f0b5254d104aab2b71c107ed661edd28`,
candidate `de247243e4fae1ce2218b6263e23af8a1b412ad633e2ecfd2e5c2adff4a635ec`.
The full actual init graph/lint belongs to runtime agent
`01a1c99e7b717018b81686624876ad5d` and ROOT. No production peer file was edited.

Current policy receipt: `3830327b6c8d7ac22452a5e32ffa32799263132c`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
implementer role, same owned leaf/session evidence, no children, runtime replay,
host cgroup writes, VM/heavy gates, profile, main merge or broader refactor.
Prior source/raw/frozen artifacts remain unchanged. Only compiler/Clippy/format
checks are needed for method removal and equivalent test field access; prior
seven-case results remain source-specific, never counted again at this source.
