# Memory review fixes, 2026-11-13

Implementer: Codex; findings memory-02..17, excluding memory-01 (Claude lane).
Policy read at base `9efde72524dd626f3ff84e6b9891852d7c810a9b`:
rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
all four policy hashes are in [the review](../../../bugs/2026-11-13/memory.md).
Fix/common briefs override the older coordinator-only merge rule: signed,
fast-forward landings after focused remote tests and a fresh-main merge.

## memory-02

Red revision `04ae8a9cd`: `cargo test -p shards-vmm memory::tests::live_copy_ --lib -- --test-threads=1`
(kiln r4) terminated in the callback test with SIGBUS (signal 10).
The page-value case alone also terminated with SIGBUS (r5):
`cargo test -p shards-vmm memory::tests::live_copy_page_keeps_bytes_after_their_block_is_unplugged --lib`.
These are real anonymous-memory plug/unplug operations, no hypervisor or mocked copy.

Fix `29196c6b3` copies a bounded live record under Access and owns returned page
bytes. Copy bounds come from the existing live wire record maximum and hotplug
block size; no timing/retry constant is introduced. The lock is released before
callbacks/stream writes, so a slow receiver cannot retain device access.
A range unplugged since discovery is skipped under the same Access used to copy.

Green at `29196c6b3`, kiln r7, adas-mac-studio.local, macOS arm64,
rustc 1.99.1: the two regressions pass; `cargo test -p shards-vmm memory::tests:: --lib`
passes 25/25; `cargo clippy -p shards-vmm --all-targets -- -D warnings` passes.
One compiler, CARGO_INCREMENTAL=0, target under ~/g121-tmp/fix-memory-v1.

R01/R04: checked page-end calculation; Access reentry is returned, not deadlocked.
R02/R05/R06/R07/R12/R13: existing Access and platform copy seam, no new manager/cache/poller.
R03: only existing record/page/block bounds; no learned policy change.
R09/R10: actual red SIGBUS and focused green above, retained in red/green logs.
R08/R11/R14/R15: no performance, matrix or real-VM claim; ROOT owes these.
R16: no builds/tests on the shared Mac; only this lane's remote target is disposable.

ROOT owes real VM validation (not run):
`cargo test -p shards moved_live --test moved` on supported KVM/HVF/WHP,
plus a pressure/unplug concurrent live-move campaign. The existing moved test
names must be selected from `crates/shards/tests/moved.rs` before that run.
