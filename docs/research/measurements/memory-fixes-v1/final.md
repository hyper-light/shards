# Memory fix handoff

Implementer: Codex, memory-02..17, excluding memory-01 (Claude P0 lane).
Current assignment is accounted for: six implemented fixes and ten owed designs.
No new review was started. Policy receipt and R01–R16 evidence are in
[README](README.md); the original severity-ordered findings and status of every
assigned item are in [memory review](../../../bugs/2026-11-13/memory.md).
Policy was re-read on resume at main a33644dbe, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, role implementer.

| Findings | Status | Fix commits | Main landing | Focused green and later gate |
|---|---|---|---|---|
| 02, 03, 05 | Fixed | 29196c6b3, e5e2ef793, fb58160ac | ab08320db | 8fea5f396: RAM 25, pmem 10, runtime 6; VMM clippy. Later main merge disjoint. |
| 04 | Fixed | 27522e6ca, aef83d2c4, 8951ffa2d | e2744a6fb | 5d822e8fe: give 10, Giver 2; shards clippy. Subsequent input-changing merges passed cargo check, ending at e2744a6fb. |
| 06..15 | Owed with designs | No production replacements | Designs landed with e2744a6fb | [Each design, cold start, typed uncertainty and ROOT command](owed-designs.md). |
| 16 | Fixed | d794c1dd5 | bfd2cca4c | 3b8ec2938: head 9, 1 measurement ignored; VMM clippy. bfd2cca4c compile check after VFIO input change. |
| 17 | Fixed | 5826a9bcf | dc23bc71f | bae738bd3: init filter 28, init clippy. dc23bc71f's later main changes are disjoint from init/dependency inputs. |

Mac checks: adas-mac-studio.local, macOS arm64, rustc 1.99.1. Linux-only init
checks: adacachy2, Linux 7.3.3-1-cachyos x86_64, native GNU target, rustc 1.98.1,
nice -n 10. One compiler, CARGO_INCREMENTAL=0, one cargo at a time; no local
builds/tests, child agents, real VMs, benchmarks, CI or mutation campaigns.
The Linux target began cold. Exact invocations, retained reds/greens, earlier
failed iterations and the fixture-only carried helper settings are in README.

Memory-17's first post-green merge 7b1d79ac3 was disjoint. 8a63a5d2b incorporated
a DNS dependency change and passed cargo check. bae738bd3 preserved both appended
PM records (memory-17 and M572) after a documentation-only conflict and reran the
28 focused tests plus clippy. dc23bc71f incorporated main 5f42c9452 automatically;
`git diff --stat bae738bd3..dc23bc71f -- crates/init crates/abi crates/archive crates/cmdline crates/template crates/devcgroup crates/dns crates/erofs crates/user crates/testdir Cargo.toml Cargo.lock .cargo`
was empty. Its later combination was not retested, per ROOT's contention rule.
All extra source changes came from main merges. No force push was used. Every
resumed source commit/merge (25 through dc23bc71f) has one signature and the
required GPT-6.1 Sol attribution.

ROOT's unrun real-VM entry points:

```sh
cargo test -p shards moves_live --test moved
cargo test -p shards --test build a_build_held_short_of_memory_has_its_builder_give_back
cargo test -p shards --test budget
cargo test -p shards --test saved a_saved_microvm_writes_on_through_its_volume
python3 scripts/check-development-policy
```

ROOT also owes actual concurrent unplug/pressure, stop during blocked precopy,
APFS map-back failure injection, budget removal behind GAVE/HELD, the head
comparison cost, cold-start research for 06..15, profiling/benchmarking, and the
supported target/host VM matrix. No performance ratio or cross-platform pass is
claimed. The exact constants/design-specific commands are in owed-designs.md.
No outside-area finding was added; memory-01 was left to its owner.

Cleanup: Mac target `~/g121-tmp/fix-memory-v1/target` removed and absence checked
(resumed r19). Linux target
`~/g121-tmp/fix-memory-v1-linux/target` removed and absence checked (resumed r24,
exit 0); `cleanup.log` retains both outcomes. Source checkouts and committed
logs are preserved. No daemon or VM was started; no cargo job remains running.

ROOT notifications, including the final dc23bc71f summary, were refused by kiln's
full inbox: 429 backpressure, pending 4, authority 3. Delivery is not claimed.
