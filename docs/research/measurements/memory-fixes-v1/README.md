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
`cargo test -p shards moves_live --test moved` on supported KVM/HVF/WHP,
plus a pressure/unplug concurrent live-move campaign. The filter selects both existing live-move tests.
Merged tree `2e8135afb`: r8 repeats the same 2/2 and 25/25 tests plus clippy successfully.
Retained logs: `red-memory-02-page.log` (r5), `green-memory-02.log` (r8).

## memory-03 and memory-05 (live-copy ownership cluster)

Resume policy receipt: implementer memory-02..17, current-main `9068a72cd`;
AGENTS/rules/evidence/CLAUDE hashes unchanged from the receipt above. The fix brief's
landing-under-contention update permits pushing a disjoint main merge without
retesting unchanged crate inputs. No child agents were started for implementation.

memory-03 red at `bebc160ab`, previous-session kiln r13, same macOS arm64 host:
`cargo test -p shards-vmm a_live_layer_read_waits_for_the_mapping_owner --lib`
failed `a read completed while release still owned the mapping: Ok(Ok(()))`.
`e5e2ef793` holds Shared.file across mapping reads and uses typed Save/Precopy
host-reader guards, releasing on errors as well as success. Teardown waits for
either reader; save/precopy cannot overlap. A retained VM Arc drops before that
guard releases. The existing mapping/flush owner is reused, with no new timing knob.

memory-05 red at `5185919b8`, previous-session kiln r14:
`cargo test -p shards-vmm a_live_copy_whose_mapping_was_lost --lib` failed
`a live copy resumed after losing its layer mapping`. This exercises the actual
post-scan continuation, extracted unchanged before fixing it; it does not inject
an actual APFS mmap failure. Fix `fb58160ac` distinguishes Mapped, Failed (intact
mapping), and Lost. Only the first two resume; Lost stops with the kept error.
Actual APFS/hypervisor fault injection and stop during blocked network output
remain ROOT's, as does cross-OS lifecycle validation. No claim that teardown
cancels a blocked stream write is made by this unit evidence.

Before memory-05's fix, r14 passed pmem's 10 tests and the new lifecycle-guard test;
its final lost-mapping regression failed as expected. Focused combined green is
recorded below, after the latest main merge.

Combined green: `8fea5f396`, resumed-session kiln r1, same Mac/Rust:
`cargo test -p shards-vmm memory::tests:: --lib` 25/25;
`cargo test -p shards-vmm devices::virtio::pmem::tests:: --lib` 10/10;
`cargo test -p shards-vmm vm::runtime:: --lib` 6/6;
`cargo clippy -p shards-vmm --all-targets -- -D warnings` clean.
The end of that run is retained in `green-live-ownership.log`.

Landing check: main through `5d805549d` changes no tested VMM input from
`9068a72cd` (crate, ABI, IPC, netring, apple, afunix, ntfs, Cargo.lock/Toml).
The `8fea5f396` test result therefore covers this disjoint merge per ROOT's
2026-11-14 landing-under-contention instruction.

## memory-04

Red at `e8b687a9e`, resumed-session kiln r2, same macOS arm64 host:
`cargo test -p shards a_budget_removed_during_an_ask_is_delivered_after_its_answer --bin shards`
failed with WouldBlock: `budget removal was lost behind the pending answer`.
This uses real socket frames and the actual budget_written/late/Giver code.
The test covers a pending GIVE_BACK and a pending ROOM reply.

Fix `27522e6ca`, typed pending state `aef83d2c4`, acknowledgment correction
`8951ffa2d`: keep the latest desired room (including None) while another reply is
out, send it once that reply arrives, and coalesce a value already in flight.
The existing late-answer regression caught deferred acknowledgment accounting
at aef83d2c4 (9/10 passed, r4); 8951ffa2d permits the caller to consume that
already transmitted room's acknowledgment. No production policy literal added.

Only this unit-test build uses SHARDS_HELPERS=skip and /usr/bin/true as carried
init/server fixture files to avoid nested guest builds. No tested IPC/budget
logic is replaced. Neither a guest nor any helper is executed by these tests.
These fixture settings provide no evidence about VM behavior.
ROOT owes `cargo test -p shards --test build a_build_held_short_of_memory_has_its_builder_give_back`
with a budget removed during a delayed give-back and delayed ROOM acknowledgment.
