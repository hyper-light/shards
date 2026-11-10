# Managed-output allocation instrumentation — test only

Base source: `24f48ee5ebf198485117aa93bf47685ad7d7d53f`. This lane implements
instrumentation for the existing ignored
`cost::owner_output_resources_at_explicit_member_count` caller. The coordinator
owns its registration in `managed_output.rs` and every actual profile, benchmark,
audit, heavy E2E, mutation campaign and combined gate. No production allocator,
installed binary, dependency, cache or semantic authority is proposed.

## Identity and scope

POLICY_READ implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Full canonical and committed checkout policy,
evidence checklist, CLAUDE, managed-runtime plan and `/Users/adalundhe/g121-handoff.md`
read. Policy revision `f5cec365`, present at base main above; rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.

Owned source: `crates/shards/tests/common/managed_alloc.rs`; only coordinator-approved
narrow hooks in `../managed-output-cost/fixture.rs`; evidence in this directory.
`managed_output.rs`, its module/global-allocator registration and tenant files
belong to other lanes/coordinator. No local builds or further full worktrees.
Any light compile uses new private sources/targets through Kiln with admission
at most 1 CPU / 1 GiB / 2 GiB. macOS is arm64 only and retains 30 GiB free.
The paused Windows ARM VM and named root clones/targets/caches remain untouched.

## Existing evidence is not allocation data

Read the committed [cost contract](../managed-output-cost/README.md),
[HOST-EXIT contract](../managed-output-cost/HOST-EXIT.md),
[f062 coordinator record](../managed-coordinator-f062/README.md) and
[exit coordinator record](../managed-coordinator-output-exit/README.md).
N0/N1/N2 were one sample per count, non-atomic and unpaired, with different
NoManagedMembers/OwnerChannelsReady contracts. They establish neither Rust
allocation counts nor retained-byte distributions. Held exit readiness and
exact fixture FD return remain separate witnesses, not memory-completeness proof.

## Primary sources and alternatives

Sources inspected 2026-11-10. Research and these standalone checks explicitly
select Rust **1.98.1**. Cargo.toml's `rust-version` is the MSRV, not a command that
selects/enforces the compiler or formatter ([Cargo contract](https://doc.rust-lang.org/cargo/reference/rust-version.html)).
Actual command/tool versions are recorded separately; these checks do not stand
for root's different closing formatter or combined matrix. Versioned source
links pin the researched contract; unversioned docs may describe a newer release.

* [Rust 1.98.1 GlobalAlloc](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/core/src/alloc/global.rs):
  unwinding is forbidden; methods require valid nonzero requests and matching
  live pointers/layouts. Null indicates failure; failed realloc retains the old
  block, whereas successful realloc transfers ownership even at the same address.
  Compiler elimination prevents interpreting source-level allocations as calls.
  Reentrancy cautions exclude allocating locks and logging in hooks.
* [Rust 1.98.1 System](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/alloc.rs):
  the official example wraps System and counts successful requested bytes.
  System supplies Unix/Windows implementation and alignment handling; direct
  native malloc/HeapAlloc cannot be mixed with its pointers. Only one global
  allocator may be registered. Runtime internals can bypass it via direct System.
* [Rust 1.98.1 Layout](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/core/src/alloc/layout.rs):
  checked constructors enforce alignment and rounded size bounds. Zero-sized
  Layout is valid even though zero-sized GlobalAlloc calls are not.
* [Rust 1.98.1 atomics](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/core/src/sync/atomic.rs):
  static AtomicUsize counters provide individual atomic operations. Separate
  counters do not form a transactional snapshot. Relaxed counters publish no
  payload or authority; checked compare/exchange updates may retry under contention.
* [stats_alloc 0.1.10 source](https://docs.rs/stats_alloc/0.1.10/src/stats_alloc/lib.rs.html):
  established small global-wrapper/region model. Its request counters update
  before backend results, so they do not distinguish failed requests. Its
  wrapping arithmetic does not provide this task's sticky unknown state.
* [dhat 0.3.3](https://docs.rs/dhat/0.3.3/dhat/): established richer System-wrapper
  profiler with backtraces and reports, documented perturbation and finite
  recording lifetime. It adds machinery outside this event-counter caller and
  does not count memory-access/copy operations. Not adopted in this batch.

The selected narrow design extends the official System wrapper pattern rather
than adopting a profiler dependency. It preserves the System contract instead
of implementing an allocator. Host behavior and perturbation remain root-owed.

## Counter contract (proposal sent before source edits)

One `ManagedAlloc` forwards every method exactly once to System. After the
backend result it records alloc/alloc_zeroed successes versus failures, realloc
successes versus failures and deallocation calls. Deallocation has no fallible
return, so no fabricated deallocation-success/native-failure count is reported.

Byte counters separate allocation requested bytes (including failed attempts),
successful allocation requested bytes, full new-size realloc requested bytes,
successful realloc growth/shrink and deallocated layout bytes. Successful
realloc updates live requested bytes by its size delta; failure leaves live
unchanged. A realloc is not double-counted as an observed alloc plus dealloc,
and no pointer movement is interpreted as a copy count.

All hooks use atomics and checked arithmetic, without heap allocation, locks,
TLS, formatting, callbacks or panic. The atomic representation reserves
`usize::MAX` for sticky Unknown; representational exhaustion or live underflow
cannot wrap or later regain a plausible exact value. This bound derives from
the counter representation, not a runtime allowance. No reset/enable window
loses pre-window allocations that are later freed. Zero initializes process
lifetime counters; literal one counts one event.

Snapshot values are always **ConcurrentNonAtomic**: individual atomic loads,
without a claimed coherent transaction, exact boundary or observed quiescence.
Live describes recorded wrapper accounting transitions; another hook may be
in flight. The process-lifetime peak is the maximum recorded live transition,
not an interval peak (subtracting peaks is invalid). A concurrent snapshot may
see live updated before peak. System can temporarily retain old/new storage
during realloc, or recycle a block before its deallocation accounting update;
this instrumentation does not measure that native physical peak.

The actual registered caller counts the **observer/test parent process only**,
including libtest, observer drivers, serialization and other instrumentation.
It cannot count the separate daemon/VM/guest, direct System/native/library
allocations, usable sizes, allocator fragmentation, RSS, kernel queues or the
whole process tree. Requested live bytes at a phase are not a leak proof.
No profile, comparison, optimization or speedup claim is made by this lane.

## Coordinator wiring and proposed phase placements

[Concrete fixture patch](fixture-hooks.proposed.patch) was approved by root and
is now applied to `managed-output-cost/fixture.rs`. The historical proposal is
preserved. `managed_output.rs` remains byte-identical to base; central module/
allocator registration is exclusively root-owned and still unrun here. The
patch adds the following fixed phase holder/JSON record without changing any
original assertion. Actual normal-helper caller compilation remains root-owed.

Root owns these exact header additions to `crates/shards/tests/managed_output.rs`:

```rust
#[path = "common/managed_alloc.rs"]
mod managed_alloc;

#[global_allocator]
static MANAGED_ALLOC: managed_alloc::ManagedAlloc = managed_alloc::ManagedAlloc::new();
```

The Linux-only actual cost caller remains Linux-only; the counter source itself
uses the same std implementation on all seven supported targets. No platform
API change or duplicate per-OS counter is needed. An empty non-Linux caller is
not portability validation.

Proposed fixture snapshots use a fixed stack holder; unreached phases are
explicitly absent. Capture before `Fixture::new`, after `start` and held exit
watch acquisition before observers, at main-progress readiness before ready/proc
record serialization, after proc record serialization, after observer FIN/cancel
and second main progress, and after unchanged shutdown/watch release/FD sampling
before teardown rendering. Persist once as `allocation-phases.json`, after the
existing teardown write but before strict assertions/rethrow. A normal final
snapshot after terminal checks and explicit fixture drop can separately show
fixture release; it is not reached on failed protocol/cleanup paths.

Intermediate record/hashing costs are deliberately included. Capturing the
readiness phase before serializing the existing records lets root identify the
observation phase without claiming to subtract or eliminate its costs. Existing
scope/bytes/offsets, identity checks, FD baselines/return, exit deadlines, owner
stop, observer join, protocol failure and shutdown behavior stay authoritative.

## Validation and coordinator obligations

Light validation will exercise event arithmetic and overflow with synthetic
results, plus small direct valid System-wrapper alloc/zeroed/realloc/dealloc
calls. Synthetic failure accounting is not an induced native OOM result. No
zero-size, invalid Layout, foreign pointer, double free or old pointer after
successful realloc is allowed. Failure assertions never make invalid calls safe.
No allocator registration is added to the actual caller by this lane.

Root still owes actual caller registration/build/lint, all supported targets,
serial real N0/N1/N2 with unchanged identity/FD/exit/bytes witnesses, finite
admitted higher counts if chosen, incomplete/error/cleanup paths and independent
counter mutations with exact restoration. Profiles need repeated paired and
interleaved equivalent workloads, raw samples and n/p50/p90/p99/max, observer
perturbation/contention, separate parent/daemon/VM/kernel costs, CPU/copies/faults,
uncertainty and native host validation. No optimization follows before root
collects and analyzes those actual data. This instrumentation closes no hard
budget, full-population/kernel return or 4–5× performance requirement.

## Actual light result and identities

Final counter source **`0183cf0683515974250f84999ae5be7bb44b585e`**, SHA-256
`7f59e4732260b41b0f9a4e47f3c1a94f73d579f9c52fbb71e4fbbb0db26c2abc`.
Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, Linux
`6.19.10-300.fc44.x86_64`, Rust1.98.1 compiler commit
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8.

Kiln admitted each invocation at **1 CPU / 1 GiB / 2 GiB**, `sync:false`.
Only these two files were digest-checked and copied to new private source
directories; no full checkout, root clone/target, carried helper or guest build.
`run-light.sh` compiles standalone tests with `rustc +1.98.1 --edition 2024
--test -D warnings -C codegen-units=1 --target <GNU-or-musl>`, then executes each
with `--test-threads=1 --nocapture`. Direct clippy-driver uses `-D warnings` and
`-D clippy::undocumented_unsafe_blocks`. Exact commands and original exits are
retained in [final raw records](records/light-0183cf068).

Final result: GNU **5 passed**, musl **5 passed**, neither ignored/failed;
both compiles and Clippy exit0. Three tests use synthetic accounting events;
two exercise small valid System wrapper blocks and cleanup. Failed-return and
overflow coverage is synthetic, not induced native OOM. No global allocator is
registered by these standalone tests; the actual cost caller is unbuilt/unrun.

| Standalone test binary | SHA-256 |
| --- | --- |
| GNU | `2ff1d97f248165d8571a3d573ae16e5217c69a55cb21488a14376d47b277c94d` |
| musl | `56a4b37cf5bbcf09d927ac0294b1716478d1b399746007c63e57b27ff8a73fdd` |

Exact remote paths are in `*-binary.sha256`; the unexecuted clippy artifact has
its own [identity](records/light-0183cf068/clippy-binary.json). Source/target/log
directories are individually named `~/managed-allocation-metrics-{source,target,light}-<full-revision>`.
They remain preserved; no active jobs or cleanup pending on this lane.

Original [1309 light records](records/light-1309b9864) retain GNU5/musl5 passes
and Clippy's missing-Default failure. The signed Default-only follow-up at
0975 passed [all light checks](records/light-0975a51c0). The final 0183 change
immediately marks peak Unknown when shrinking/removing live accounting becomes
Unknown, with an added focused assertion; this is recorder implementation,
not a reproduced product issue or a performance fix. Its light rerun above is
the current-source result. No lint or safety condition was waived.

Each transfer validates archive hash and regular relative paths and retains
per-file hashes and original logs without normalization. Archive SHA-256s:
1309 `93fbf97372043c6b3e32f32f1dd3df818e52d6b4ba7d2891dad90c258c26cb13`;
0975 `5346936732b3c4939b9f6e2d03fc6e809ec7568fba82ac692239e692539ad287`;
0183 `78fcfc69711f803de720ef6f0dd034debc71c580ec50789dfa3f594997d87333`.
Original archives and binaries remain on the runner. The records contain no
actual resource samples or comparative distributions. Local formatter, policy,
shell syntax and unapplied patch checks are source-only checks.

Staged whitespace checking flags blank EOF lines in seven original test/lint
logs and literal blank context lines in the unapplied unified patch. They are
retained byte-for-byte and classified in `records/WHITESPACE.json`; the source
and other staged paths check clean. No raw log, global Git rule or lint is
rewritten/relaxed to hide these artifact-format findings.

R01/R04/R06/R07/R13: test-only checked accounting and explicit Unknown; root code
review/actual checks pending. R02/R03/R05/R12: primary contracts, alternatives,
one real caller, representation-derived bounds, no cache/probe/platform manager.
R08/R14: all performance is unmeasured and root-owned. R09/R10: measurement gap
motivates instrumentation, not a guessed product fix; durable light evidence is
linked above. R11: GNU/musl standalone checks only; other targets, actual caller
and real supported VM hosts remain unverified here.
R15/R16: signed named-path checkpoints, private admitted light scope, no children,
main merge, heavy run, root-cache write or destructive cleanup.
