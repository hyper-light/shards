# MR-OOM-MAC-1: current-source synchronization decision precursor

**No remedy or architecture selected. No production edit or native probe.** This
record prepares ROOT's next current-source reproduction and implementation choice.
The retained [coordinator record](../managed-sync-coordinator/README.md) remains
an open obligation; warming, a spinlock or an earlier infallible allocation does
not discharge it. The notification fixture/failure launcher and all prior evidence
remain frozen. The IOMMU proposal remains unapproved.

POLICY_READ implementer `01a1c5bda0bf766683384c0705c0a9bc`; coordinator
`01a148884a4770fdaf0111f3f792a248`; full canonical AGENTS/R01–R16/evidence/CLAUDE/
managed plan/current handoff and FULL MR-OOM-MAC-1 README read before work.
Requested source comparison: `eb5654d6f5356cacaca536ff51f14a632fa7e801`; rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
On resume, canonical main/origin was `f9030566bc227d50770bba879bd1387ebffe410f`;
full current policy/plan/handoff were reread. Plan SHA-256
`8b3fcc78cc5c112e2c6405c234c740a12ed2b38b35c7feedcaebe7a21d84be8e`, latest full
handoff SHA-256 `f3b15a3de551b4b0ebea82ef040563d37c911c18120257ab653637303e655e6e`.
The identity record preserves the initial receipt as well as the resumed receipt.
Ownership: this new proposal, source identities and checked interface example only.
ROOT owns native/OOM/contention/fault/profile/matrix/campaign execution and merges.

## 1. What was actually reproduced, and what changed

The coordinator's actual owned-connect abort belongs to source
`122332036eb689c33fd52b4a065c53c7dc5e4530`, macOS arm64/Darwin25.4 and Rust1.98.1
commit `48a229ceaefd4985c50990b14116b6d856af0985`. Its ordinary test aborted;
a separate test-allocator diagnostic retained the refused 64-byte request and
`OnceBox<...Mutex>::initialize -> ShardsGuest::connect` stack, then also aborted.
The diagnostic was not a production modification, cost benchmark or fixed result.
The separate cold-mutex observe/refuse/warm cases establish the narrower recorded
attribution; a warm control is not a remedy.

[Exact source identities and span comparisons](source-identity.json) retain URLs,
Git blobs, full SHA-256, line ranges, retrieval time and failures to retrieve.

| Named source | Reproduced revision | Current eb5654 |
| --- | --- | --- |
| `crates/api/src/client.rs` | blob `b6045d0718ec4bf112bc4539fcc7c3477bc7dd57`; SHA-256 `0dada72aefc5f1eb605c4938d9023b35d5ffa3b2bd7f748ab9e124b5c5b60794` | blob `5c397ae3a9f0ba36e514a63a2ce44308e243a77c`; SHA-256 `34b6943af5831d1bc0e8341d5ea021189d737e78cd8d0114c18ec0a8543770ce` |
| Shared/State/Tail; lock helper; boot construction; watcher/drain/Child; handle; terminal/signal; connect prefix through first State lock; stop/identity/wait/drop | Exact named spans retained | All eight spans byte-identical; whole file is not identical |
| Handshake and refusal path | `channel::handshake`, typed allocation mapping, fallible socket boxing | Formatting/`Vec::with_capacity` in handshake, `Box::new` on success; old allocation mapping absent |
| Old owned OOM test | Present | `owned_connection_oom_is_typed_before_error_formatting` absent |
| API refusal and allocator wiring | Borrowed/channel candidate and its allocation test helper | `Refusal::Process` exists; no allocation/sync-init variant. `proc_allocation` is Linux-test-only; no current Mac allocator hook |

These are **source facts**, not a claim that current eb5654 passed or failed a
native case. Replaying the old `--exact` test name on current selects no such
test. An exit0 with zero tests is not a reproduction. Current source retains the
cold first State lock, but a current red, its exact allocation/stack and its
cleanup remain owed. Other current String/Vec/Box/Arc/thread allocations are not
certified by the historical Mutex result or by a future isolated Mutex fix.
All twelve named eb5654 source/evidence files in the identity record also have
identical Git blobs and SHA-256 at f9030566; that later-main comparison is source
identity only. It does not transfer any historical native outcome to f9030566.

Current [rust-toolchain.toml](https://github.com/hyper-light/shards/blob/eb5654d6f5356cacaca536ff51f14a632fa7e801/rust-toolchain.toml)
actually pins 1.98.1; this is distinct from Cargo's MSRV. A standalone example
checked by a different compiler must identify it and cannot stand in for that pin.

## 2. Minimum real users and invariants

This is the named API caller slice, not a repository lock audit. Source anchors:
[construction](https://github.com/hyper-light/shards/blob/eb5654d6f5356cacaca536ff51f14a632fa7e801/crates/api/src/client.rs#L159),
[Shared/State/Tail](https://github.com/hyper-light/shards/blob/eb5654d6f5356cacaca536ff51f14a632fa7e801/crates/api/src/client.rs#L375),
[watch/drain](https://github.com/hyper-light/shards/blob/eb5654d6f5356cacaca536ff51f14a632fa7e801/crates/api/src/client.rs#L430),
[owned API methods](https://github.com/hyper-light/shards/blob/eb5654d6f5356cacaca536ff51f14a632fa7e801/crates/api/src/client.rs#L829).

| Existing object | Actual users / obligation |
| --- | --- |
| State mutex + `changed` CV | Owned `connect`, `stop`, `identity`, `wait_for`, Drop, owned reaper and Unix taken-process watcher. `Guest: Send + Sync` permits concurrent callers. `changed` pairs only with State; terminal predicates are checked under its guard. |
| Tail mutex + `drained` CV | Drainer appends/truncates retained output, marks completion and retains the pipe reader. Owned reaper writes its existing end marker, waits for drain and copies the tail. `drained` pairs only with Tail. Pipe reader retention prevents a marker write meeting a closed pipe. |
| Child mutex | `Exit::of`, thread-start rollback, reaper; Windows stop/identity also access it. Unix Guest methods signal its still-unreaped PID instead. Thread-start failure must retain kill/reap authority. [Child has no automatic kill/wait on drop](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/process.rs); moving and dropping its handle is not cleanup. |
| Attached Unix variant | Held PID/start identity, State/changed watcher; no owned Child or active tail-drain worker. Its initialized ready/drained values and non-parent exit-status semantics differ from a newly booted guest. |

The real held order includes **State -> Child** for reaping and Windows control.
Tail is released before State is acquired; Child's `Exit::of` guard is released
before workers start. Do not introduce Child -> State or Tail -> State held edges,
or callbacks/I/O that reenter those guards. Reaping and publishing `State.exited`
occur under State: moving either outside that arbitration can invalidate signal
identity. Preserve `taken::Held`, PID/start/socket identity, existing grants/leases
and generation semantics; synchronization cannot enlarge authority.

The current terminal owner is State: stop intent, readiness, exit status and cached
End classification are serialized there. Tail has a separate output-drain
predicate. Any later representation/ownership change must replace the existing
authoritative representation, not add mirrored atomic flags or a parallel phase
ledger. No new manager/actor/cache is proposed here. A wake is a reason to recheck
the authoritative predicate, not a second terminal fact.

Preserve atomic unlock-and-wait, guard reacquisition, predicate loops, notify/wait
races, timeout/spurious-wake behavior and stable mutex/CV pairing. Preserve the
existing connect zero/MAX semantics and stop-as-no-leak contract. A late lock or
wait error cannot simply refuse stop while abandoning a running guest or release
its charge on uncertain exit. Those are design/real-proof requirements, not
properties established by this proposal.

## 3. Primary alternatives and their boundaries

Rust source below is pinned to the **actual repro compiler commit**, not a moving
documentation version. The seven supported targets select:

| Targets | Rust1.98.1 source selection; not measured cost |
| --- | --- |
| macOS arm64 | pthread Mutex/Condvar wrappers with OnceBox-backed native objects |
| Linux GNU + musl, amd64 + arm64 | futex Mutex/Condvar wrappers |
| Windows MSVC amd64 + arm64 | default Windows futex branch, not the separate `win7` vendor branch |

[Mutex selection](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/mutex/mod.rs),
[Condvar selection](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/condvar/mod.rs).
Neither source selection nor an SDK size proves runtime allocation freedom,
bounded contention, kernel resource return or a passing target.

| Alternative | Concrete compatibility/refusal/cost question; no selection |
| --- | --- |
| Existing std locks and Shards helpers | [API lock](https://github.com/hyper-light/shards/blob/eb5654d6f5356cacaca536ff51f14a632fa7e801/crates/api/src/client.rs#L424) and [vmm::sync](https://github.com/hyper-light/shards/blob/eb5654d6f5356cacaca536ff51f14a632fa7e801/crates/vmm/src/sync.rs) recover poison, not failed allocation. [pthread Mutex `get`](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/mutex/pthread.rs) uses `Box::pin` for lock and try-lock. [Condvar `get`](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/condvar/pthread.rs) is reached from notify as well as wait. Warming/try-locking earlier or reserving an unrelated 64 bytes is not fallible initialization. |
| A concrete fallibly prepared owned-control block | Potentially preserve existing callers with one private owner and typed preparation before publication. Requires stable, correctly aligned storage, actual native init return handling, prefix cleanup, wait/notify/lock failure semantics, quiescent destruction and current caller proof. [Rust's native pthread wrapper](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/pal/unix/sync/mutex.rs) explains pinning/relocking and itself unwraps init errors; it is private std implementation, not an importable fallible API. Native platform adapters are conditional on ROOT proving necessity after alternatives/live data; not approved duplication. |
| Stable fallible allocation facilities | [Box::try_new](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/alloc/src/boxed.rs) and [Arc::try_new](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/alloc/src/sync.rs) are allocator_api-unstable at the pinned toolchain. A stable concrete allocation route would need checked `Layout`/null/ownership/deallocation evidence, not a copied byte size or private Arc layout. The historical socket-boxing candidate is not adopted current code. Other existing Arc/Vec/String allocations remain separate obligations. |
| triomphe 0.1.17 fallible shared allocation | [Published implementation](https://raw.githubusercontent.com/Manishearth/triomphe/1c3cfa221b3f97729cbc989fc67976f45f874331/src/arc.rs) provides `Arc::try_new` with checked layout extension and a null-to-AllocError path; its package checksum and exact raw source match are recorded. This addresses shared-storage admission, not std's separate lazy Mutex/CV allocation. Its constructor consumes `T` and does not return it on allocation failure: consequently, passing an already-live Child directly would lose that handle on refusal, unless an existing retained owner supplies cleanup. This is an ownership inference from the constructor and Child contract, not a reproduced leak. Its reference-count overflow abort also requires a real bounded-owner argument; library choice alone cannot prove R01. No dependency adoption or candidate-branch runtime result is claimed. |
| parking_lot 0.12.5 and 0.13.0 | [0.12.5 parking core](https://raw.githubusercontent.com/Amanieu/parking_lot/d7828fff7b5d6327ae608e82db45f888b344449a/core/src/parking_lot.rs), [0.13.0 core](https://raw.githubusercontent.com/Amanieu/parking_lot/1a5c3dd5c328b574234082e3ae4d8de078090d87/core/src/parking_lot.rs): inline lock state does not remove infallible global table construction/growth or first-thread parking preparation. Old tables are retained by that implementation. [Condvar](https://raw.githubusercontent.com/Amanieu/parking_lot/1a5c3dd5c328b574234082e3ae4d8de078090d87/src/condvar.rs) and [mutex](https://raw.githubusercontent.com/Amanieu/parking_lot/1a5c3dd5c328b574234082e3ae4d8de078090d87/src/mutex.rs) also differ from std in poisoning/wakeup behavior. A cold uncontended pass would not establish first-contention refusal or an admitted global-table lifetime. No dependency is present in current Cargo.lock and none is added. |
| atomic-wait building block | [Pinned API](https://raw.githubusercontent.com/m-ou-se/atomic-wait/6a42cf2b8395e9e9a6c44e689f3b9760d6239cfa/src/lib.rs) exposes void atomic wait/wake, not a timed mutex/guard/CV/refusal contract. [Mac path](https://raw.githubusercontent.com/m-ou-se/atomic-wait/6a42cf2b8395e9e9a6c44e689f3b9760d6239cfa/src/macos.rs) links libc++ monitor/wait symbols; its ABI-compatibility claim is not deployed-library/cost proof. [Linux](https://raw.githubusercontent.com/m-ou-se/atomic-wait/6a42cf2b8395e9e9a6c44e689f3b9760d6239cfa/src/linux.rs)/[Windows](https://raw.githubusercontent.com/m-ou-se/atomic-wait/6a42cf2b8395e9e9a6c44e689f3b9760d6239cfa/src/windows.rs) calls do not expose native wait errors as a typed Result. Completing a new generic Mutex/framework on top is not approved. |
| Crossbeam Parker / owner-thread messaging | [Parker 0.8.21](https://raw.githubusercontent.com/crossbeam-rs/crossbeam/ccd83ac4108a2a1b41e9c6e79c87267167d18dfa/crossbeam-utils/src/sync/parker.rs) contains Arc and mutex/CV-backed parking; a permit is not State mutual exclusion. Moving all state to the existing watcher would also need synchronous multi-caller requests while that watcher awaits process exit, plus fallible channel/admission/stop progress. No second runtime owner or actor framework is justified by this record. |

The pinned [Apple mutex](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/1f4f5265b319111142f1bf3a27d4484ef5a98314/src/pthread_mutex.c),
[condition variable](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/1f4f5265b319111142f1bf3a27d4484ef5a98314/src/pthread_cond.c)
and [types](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/1f4f5265b319111142f1bf3a27d4484ef5a98314/include/sys/_pthread/_pthread_types.h)
are a reference implementation, **not a verified match to the deployed dylib**.
The locally read SDK26.4 header path/hash is retained separately. Its LP64 opaque
Mutex/Condvar declarations explain layout arithmetic; they are not allocator
requests, retained memory budgets or a proposed production literal. The observed
64-byte request remains the old actual trace, not a replacement storage default.

The [OnceBox implementation](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/once_box.rs)
and Mutex initializer permit competing initialization attempts; a losing newly
allocated native object is destroyed. A single cold allocation sample therefore
does not bound concurrent initialization peaks. The Mutex wrapper also preserves
a locked allocation if its guard was leaked. A future concrete owner must state
its guard lifetime, same-thread unlock, CV pairing and quiescent-destruction
obligations rather than inherit a leak-free claim from std or a source layout.

| Target family | Initialization / refusal / cleanup obligations still requiring ROOT proof |
| --- | --- |
| macOS arm64 | Cold lock/try-lock and first CV notify/wait can reach std's infallible pinned allocation. Native attribute/init/lock/wait errors do not become a typed Shards refusal through poison recovery. A different mechanism must prove actual fallible preparation, stable address, original monotonic timeout and safe partial destruction; deployed C/runtime/kernel allocation remains unknown. |
| Linux GNU/musl, both architectures | Selected std Mutex/CV state is inline futex state, not the pthread OnceBox source path. This does not establish that State/Child Arc construction, thread start, first parking, kernel admission or cleanup cannot fail. Both libc/architecture selections need actual current caller/failure tests; a Mac-only attribution is not a Linux passing gate. |
| Windows MSVC, both architectures | Selected std path uses futex-style atomic state with Windows platform waits; it is not evidence that an SRW replacement was measured or selected. Preserve owned Child/process-handle lifetime and the existing State-to-Child ordering. Source selection is not a first-wait, OOM, handle-return or WHP result; ARM VM remains paused. |

Native errors must keep their actual operation/code namespace without allocating
a String in the refusal path. Current Refusal lacks that sync-init variant; the
existing typed ProcessFailure is a representation pattern, not an appropriate
substitute cause. A public extension needs exhaustive-caller/compatibility review.
Raw native initialization and destruction must not be copied with std's unwraps.
The [Apple timeout path in Rust](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/pal/unix/sync/condvar.rs)
also carries monotonic relative-time and representability logic; replacing it with
a realtime or reset timeout is not a neutral lock change.

## 4. Checked interface obligation, without a primitive implementation

[interface.rs](interface.rs) uses concrete owned-sync failure data and the **real
Child type** to check two call shapes: preparation refusal prevents launch; a
failure after acquiring Child returns its ownership for the existing kill/reap
path. Failed launch also returns prepared resources for explicit cleanup. No
generic Mutex, native call, guard, new phase ledger, lifecycle manager or chosen
storage/backend is implemented. The sealed mock preparation receipt is not an
initialization, pinning or Send/Sync proof. The error's Display writes to a supplied
formatter; it does not promise that a caller's formatter/reporting path allocates
nothing.

This distinguishes fallible preparation from warming an infallible std lock. Any
future initialization failure must occur before publication or retain exact held
ownership until cleanup/uncertainty is resolved. A partial-init resource tracker
is ownership of initialized resources, not a second copy of guest terminal state.
The actual primitive/guard/CV implementation and successful real resources are
deliberately absent from this example.

[Exact example commands, raw outputs and identities](example-check.json) retain
two NEW private Fedora directories. The final directory is
`/home/adalundhe/managed-cold-sync-example-v2-lmrc9y4h`, admitted with
`sync:false`, one CPU, 1 GiB memory and 2 GiB disk; affinity/AS/file-size limits
are recorded, with no claim of a kernel aggregate disk quota. Installed Rust
1.98.1 was the exact `48a229ceaefd4985c50990b14116b6d856af0985` compiler and
LLVM22.1.8. Final rustfmt, metadata type check and two serial pure tests returned0
with warnings denied. `--cfg duplicate_owner` returned the expected1/E0382 for
reusing the moved real Child type. Interface SHA-256:
`fc04c6bc94f41e6f0e7973139d25c956bf00540ef4bfa30d5b4b914dcf76b43d`.
No Child was constructed, process launched by the example, mutex/backend probed
or OOM injected. The initial check's `cfg(test)` allowlist warnings remain in the
record; correcting the command and rerunning removed them without a code fix.

## 5. Minimal ROOT reproduction and proof recipe — entirely unrun here

1. Admit a NEW private exact-current source/target and finite case/outer budgets,
   preserve frozen binaries/evidence, keep Mac arm64 floor/lock rules. Record exact
   Rust1.98.1 identity, SDK/dylib facts available, environment, source diff and all
   resulting binaries. Do not use the old instrumented binary as a current build.
   This record's reviewed source basis is eb5654, byte-compared through f9030566;
   later main must first repeat that named-file comparison and retain its own diff.
2. First list the exact new private test and require **one selected test**. The old
   name is absent. Add a test-only current-source case that actually enters owned
   `Guest::connect`, using the existing owned-child fixture topology and declared
   allowed port. Setup/output/identity reporting must not call a Guest method that
   warms State before injection. Record its owned child for independent outer
   cleanup if the test process aborts; Child Drop alone is insufficient.
   A proposed NEW selector is
   `client::tests::owned_connection_cold_sync_refusal_current` (not implemented
   here). After ROOT approves and registers that test, the planned command array is
   `["lockf", "/tmp/g121-mac.lock", "cargo", "test", "--locked", "-j4",
   "--profile", "test-release", "-p", "shards-api", "--lib", SELECTOR, "--",
   "--exact", "--test-threads=1", "--nocapture"]`, with an explicit NEW
   `CARGO_TARGET_DIR` and finite outer supervisor. First use the same test selection
   with `--list --exact` and require exactly the named entry; retain both commands,
   selected count and outputs. A compile or selection failure stops the recipe.
3. Use exactly one private test allocator. Current proc_allocation is cfg(test,
   linux), not a Mac hook. In the private Mac source, arm a thread-local null
   refusal immediately around the actual entry, after ordinary setup; keep
   diagnostics nonallocating at that boundary. First retain an unmodified red,
   then a separate attribution artifact if needed. Preserve signal/exit and
   actual first-failed request/stack; compile failures or zero tests are not red.
4. Keep the historical 64-byte targeted probe only as attribution. Also refuse
   the first actual allocation without a size whitelist so a different-sized
   hidden allocation cannot become a false green. A new caller may reach another
   allocation after the old one is removed; retain that different failure rather
   than weaken the guard or claim this record certified the whole API.
5. Only after current red and ROOT decision, admit a narrow implementation.
   Refuse each real preparation stage before publication and first contention,
   each CV's first notify/wait, constructor races and partial cleanup. Successful
   old-style first connect must not merely depend on a prior infallible warmup.
   Assert typed returned cause and actual child/handle/resource return, not only
   an error label or absence of Rust-global allocations.
6. Exercise stop/reap/identity/wait/connect concurrently with authoritative
   predicates and original deadlines, tail end-marker/EOF cases, thread-start
   failures and attached-process behavior. Retain meaningful actual guard removal
   and exact restored passes; all races/campaigns are ROOT execution, not delegated.
7. Run the actual seven-target gates and required native hosts after composition.
   ROOT compares cold/uncontended/first-park/contended behavior and Rust/C/kernel/
   parent costs against equivalent pinned controls, with raw distributions and
   misses. A warm counter, SDK size or source count is not that result.

Future paths for ROOT assignment, **not edits authorized by this proposal**:
current-source test/diagnostic fixture under this measurement record plus narrow
cfg(test) API registration; `crates/api/src/client.rs` constructor/control binding;
`crates/api/src/lib.rs` only if a new heapless typed refusal is approved. A new
private concrete owned-control implementation is conditional on the selected
mechanism and proven necessity; `crates/api/src/owned_sync.rs` is a possible
private binding path only if ROOT approves that alternative, not an agreed module.
No shared generic Mutex crate or dependency fork
is preselected. Existing stream/handshake allocation changes require their own
actual-current failure and scoped approval; they are not bundled speculatively.

## 6. Evidence limits and R01–R16

Source identities and the checked example are evidence of reading/type/control
shape only. Native current red/green, allocation refusal, contention, cleanup,
latency/CPU/fault/retained/kernel charge and all target/hardware results remain
**UNRUN by this lane**. No 4–5×, allocation-free, leak-free, R01 closure, managed
plan readiness or lane release is claimed. ROOT chooses after real data.

| Rule | Evidence / gap |
| --- | --- |
| R01 | Historical actual fatal allocation remains open; no production edit. Example carries heapless causes and retained ownership, not a working panic-free primitive. Current native/failure/cleanup proof owed. |
| R02 | Exact current/repro comparison, pinned primary/established alternatives, invariants and required real data above. No method selected by familiarity/source count. |
| R03 | SDK/layout/source facts separated from measurements; no new numeric limit/default. Required future storage derives from actual types/layout/ownership, with allocation refusal and overflow proof. |
| R04 | Current authoritative predicates/lock order and typed error/ownership obligations explicit. No mirrored booleans, cached phase or new authority created. |
| R05 | Existing caller/synchronization/identity seams considered; no new cache/poll/spin/warming/manager. Candidate necessity and contention data remain ROOT decisions. |
| R06 | One focused proposal and concrete call-shape example; no repository-wide audit/refactor/framework. |
| R07 | Comments distinguish mock/type evidence, held Child and missing native implementation; no layout-as-runtime claim. |
| R08 | No comparable cost/distribution/ratio achieved; ROOT controls and misses required. |
| R09 | Old actual reproduction retained; current source differs, current reproduction owed before a production remedy. |
| R10 | Source identities/deltas and repeatable current recipe retained; no fabricated native trace or inherited passing gate. |
| R11 | Seven-target source selection accounted for; actual gates and Mac/current native probes unrun. Intel Mac unsupported; Windows ARM VM remains paused/unverified. |
| R12 | Shared existing semantics preferred, conditional smallest adapter only after necessity/live evidence; no new installed binary, privilege, security or crypto change. |
| R13 | Real minimum callers and concrete ownership/refusal example; no generic synchronization architecture selected. |
| R14 | No profiling/measurement by this lane. All Rust/C/kernel/parent/copy/fault/contention attribution remains ROOT-only. |
| R15 | Sole writer, no children/goal-plan tools/main merge. Signed checkpoint is review progress; ROOT owns real campaigns and future assignment. |
| R16 | Source/small text only locally; example checks only in NEW admitted <=1CPU/1GiB/2GiB remote dirs. No local heavy build/full copy/cache deletion; root caches/frozen evidence/paused VM preserved. |

The sustained lane stays assigned until ROOT releases it. Frozen notification
checkpoints remain `edb25dd4f8ff5994a05f4da2015d9021f2747e26` and
`f334c4e75ab7e9149df2103b90d3d6a72b39b6b4`; no active local target/cache/job.
After compaction reread current canonical policy/handoff and preserve this narrow
scope. No child/auto-planning dispatch or unapproved IOMMU implementation.
