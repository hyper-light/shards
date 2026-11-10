# MR-OOM-MAC-1 current-source red and implementation handoff

The assigned production correction is **in progress**. This first checkpoint
retains a new actual native reproduction; it does not select or ship a remedy.
The [earlier proposal](../PROPOSAL.md) and primary alternatives remain the design
basis. The old122332 result is not used as a passing/failing gate for new source.

POLICY_READ implementer01a1c5bda0bf766683384c0705c0a9bc; coordinator
01a148884a4770fdaf0111f3f792a248; canonical current main
`cbd39284937d024640c56db6f8d8ef2bd9b0ee24`, full AGENTS/R01–R16/evidence/CLAUDE/
managed plan and full handoff read. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
handoff read snapshot `96ccd082d2dbe434bb9fc958866df00af9452bc0caae5b77ab8a68f4b0e8c5ec`.
Its historical coordinator-only/d634 text is superseded by the current explicit
assignment and policy. This was the specifically assigned bounded native
allocation-refusal correctness test, not profiling, a sweep or heavy real E2E.

## Actual result

Native ARM64 Mac runner `d1323898b255a3ed435eb83fa0f7b0cf`, installed Rust1.98.1
commit48a229ceaefd4985c50990b14116b6d856af0985/LLVM22.1.8, private
`/Users/adalundhe/managed-cold-sync-current-cbd-v1-eld3erzq/execution-v3`:

| Exact source closure | Actual build / selection | Ordinary and diagnostic allocation refusal |
| --- | --- | --- |
| Main cbd39284937d024640c56db6f8d8ef2bd9b0ee24 | build0; exactly one named API library test | both SIGABRT(-6), first refused allocation64B |
| Candidate d086894864cd5b3152fbf17b355186f99824155b | build0; exactly one named API library test | both SIGABRT(-6), first refused allocation64B |

Both diagnostic stacks contain the actual OnceBox native pthread Mutex initializer
followed by ShardsGuest::connect. The ordinary case has no backtrace hook enabled;
the separate diagnostic retains null refusal after writing its fixed stack.
C unwinding costs are unknown and it is not production instrumentation.
See exact [current stderr](raw/current-cbd392/attribution/stderr),
[candidate stderr](raw/candidate-d086/attribution/stderr), and the ordinary logs
beside them. Collector exit0 means records were collected; it is not a native pass.

Current binary SHA256 `0e878a3f1149f92a4188a156ee68b009dbbdb05368d8f5681e6cb75e3f6102aa`;
candidate binary SHA256 `67ecfeb5a02e14f79f708159c8ba9e0782c19362ae427bdd93730134f8545aab`.
All original source and binaries remain in the private remote scope.

## Repeat and provenance

`prepare.py --out NEW_PRIVATE_DIRECTORY` exports exactly four crates from the
two named Git revisions. All crate source bytes are copied, not mocked. Only the
private workspace membership/unused unrelated patch directories, lock dependency
closure and appended cfg(test) registration differ; the actual original production
client bytes remain an exact prefix. No product Cargo.lock/dependency/TLS/ABI
change was made. The d086 focused test modules are also retained for compilation.

Transfer its verified bundle and `run.py` into a NEW admitted ARM64 Mac directory;
invoke the [exact recorded driver arguments](raw/driver/command-v3.json). Source
export identities, derived manifests/locks, compile commands/raw outputs, exact
selection and every case's arguments/status are in each retained summary. Run
`cargo +1.98.1 test --locked -j1 --profile test-release -p shards-api --lib --no-run
--message-format=json`, then the selected binary's exact named test, as the driver
records. This is a small API closure, not the normal full workspace/helper gate.

Each case uses actual Guest::start with one finite sleep stand-in and an owned
socket listener. No Guest identity/stop/wait/connect call warms State before the
fault. The supervisor creates a fresh test session and holds its test process
unreaped; it registers the stand-in's NOTE_EXIT observer before releasing the
allocation gate. All allocations on the calling test thread are refused, without
a size whitelist. The candidate's typed-allocation assertion would run only if
connect returned; it was not reached in these aborts.

All four tests and stand-ins produced actual exit notifications, no case deadline
expired, each test process was actually waited, and only marked owned socket/
marker/directories were removed. Grandchild reaping after the test abort remains
unknown: an exit notification is not a reap or kernel/resource-return proof.
No child/VM belonging to another owner was addressed.

Envelope: Kiln `sync:false`, one CPU/1GiB memory/2GiB disk admission; cargo-j1,
LLVMthreads1 and codegen1, case15s, compile180s, outer600s. Actual preflight free
2473959137280B exceeded32GiB before the2GiB request. 1GiB is admission, not an
enforced Darwin RSS/data quota. Initial v1 and v2 launchers failed **before any
compile or test** because Darwin rejected RLIMIT_DATA setters; sources/errors
are retained in launches.json and raw/driver. The subsequent read-only maxdsiz
query was unavailable; no host limit, sysctl or security setting was changed.
v3 retained inherited data limits, zero core limit and a child-only file-size cap.
No compiler/fixture memory profile or aggregate disk quota is claimed.

Original archive26951B/36 regular members SHA256
`eb3c6df11f78e15db76aa01bdfa909452dd3d87f09bd09048e83304b9cf5dcb0`;
archive and every [declared member hash](raw/manifest.json) were independently
verified before retaining raw text here. No executable is committed.

The executable-matching test source is retained under raw; the top-level replay
source was subsequently rustfmt-formatted. The named source/Python/policy checks
pass. Full staged whitespace checking reports only an extra blank EOF in each
of the two byte-exact raw derived Cargo.lock artifacts; those originals are
preserved and no whole-diff whitespace pass is claimed.

## Next production binding boundary

ROOT was sent exact proposed paths before a broader rewrite: new private
`crates/api/src/sync.rs`; client.rs synchronization imports/helper and owned/taken
preparation/wait bindings; lib.rs module and heapless typed preparation refusal
only as required. No borrowed handshake/deadline/afunix/parser/server/init/net
work is owned. The d086 lane owns those adjacent API changes.

A real constructor must fallibly allocate stable storage, check native init
returns, destroy only initialized resources and return any already-owned Child
on refusal. Warming std, an earlier infallible Box, spin/poll/cache or a duplicate
phase manager is excluded. Runtime lock/wait failure cannot create an unheld
Guard or silently abandon the stop/reap contract. That contract and the common
platform interface are being resolved before caller binding. Native current
red is now established; no fixed green or architecture choice is claimed here.

## R01–R16

| Rule | Actual evidence / remaining obligation |
| --- | --- |
| R01 | Current/candidate fatal64B reproduced. Production correction and typed/failure/cleanup green remain owed. |
| R02 | Prior pinned alternatives plus exact current real red; primitive necessity/error/ownership proof in progress. |
| R03 | No production constant selected. Fixture envelopes are caller inputs;64B is observed attribution only. |
| R04 | Existing State remains authority. Gate/held process/observer/refusal/cleanup states recorded; no mirrored runtime phase state. |
| R05 | Three actual mutex/two CV callers; no cache, polling/spin fallback or new manager. |
| R06 | Small API closure and one selected fixture; not a repository audit. |
| R07 | Safety/ownership and diagnostic limits documented; original raw source remains exact. |
| R08 | No performance/comparative result, ratio or cost allowance claimed. |
| R09 | Four new actual SIGABRT results and initializer/connect stacks at the named current revisions. |
| R10 | Exact commands/source/binary identities, raw failure/cleanup records, retained prior harness errors. Fixed green/mutation owed. |
| R11 | Native Mac arm64 correctness only; all seven-target/full/native VM gates remain owed to ROOT. Windows ARM VM stays paused. |
| R12 | No product/backend/privilege/security/crypto change or extra deliverable in this checkpoint. Shared primitive necessity being resolved. |
| R13 | One finite existing-seam fixture; no framework or duplicate authority ledger. |
| R14 | Allocation refusal is correctness attribution, not total allocation/C/kernel/CPU/fault profiling. ROOT retains profiling. |
| R15 | Explicit bounded correctness assignment, sole index writer, no children/goals/main merge. ROOT owns final integration/matrix/review. |
| R16 | No local compile/target/cache; two small local transfer directories preserved. New private remote sources/targets only; protected caches, frozen bafc/f334 and paused VM untouched. |

This is a sustained implementation checkpoint. The production-correction task
continues; no lane completion or release is claimed.
