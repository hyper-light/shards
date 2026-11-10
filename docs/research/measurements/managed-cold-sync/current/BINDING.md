# MR-OOM-MAC-1: real owned-API State/Tail binding

The real State/Tail callers now use fallibly prepared synchronization. Exact
main33df first reproduced the cold abort; the bound API then passed the two cold
allocation-refusal cases and five ordinary compatibility cases on ARM64 macOS.
This is a scoped implementation checkpoint for ROOT review. The seven-target,
combined/native-error and performance gates remain owed; the lane stays assigned.

## Policy and source identity

POLICY_READ implementer / assigned-adversarial
`01a1c5bda0bf766683384c0705c0a9bc`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Full canonical AGENTS, R01–R16, evidence,
CLAUDE, managed plan and `/Users/adalundhe/g121-handoff.md` were reread before
edits/tests at main `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`. Rules SHA-256:
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

Own `crates/api/src/sync.rs`, client State/Tail constructors/synchronization,
lib private registration/typed refusal, and this `current/` evidence directory.
No borrowed handshake/deadline, stream, afunix, server, init or net implementation
changed. The full span from `Attempt` through the borrowed implementation is
byte-identical to main33df, SHA-256
`20ccf7c14cfe86a82a5ac6f1e2dfcb29118055592c93b21e61ef5ed3d800f7e1`.

New branch `p/managed-cold-sync-owned-v1` starts at exact main33df. Original
`p/managed-poller-research` / `25f92e6049bff47a8cfee5e23a12168c25cd81b0`,
f39, bc63, the constructor-owner draft and frozen notification artifacts remain
preserved. Their earlier results are not relabelled current-source results.
Selected own proof commits were carried onto this branch; ROOT reviews the
focused product diff against main33df rather than adopting old research ancestry.

## Basis and implementation

The [pinned alternatives and identities](../source-identity.json),
[proposal](../PROPOSAL.md) and [native backend record](primitive/README.md) retain
the prior extensive primary-source comparison. The live current33df ordinary
and diagnostic cases both aborted with first refusal64B; the diagnostic stack
is `OnceBox<unix Mutex>::initialize -> ShardsGuest::connect`. Compile0 and exact
selection1 precede those actual failures. The baseline binary SHA-256 is
`8b870a544008295ae150c7df07b5c8694fd6e022e1e840d1bbe3d6b4aebe992e`.

Relevant primary sources remain pinned to the actual Rust compiler:

- [Rust mutex selection](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/mutex/mod.rs),
  [pthread mutex wrapper](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/mutex/pthread.rs)
  and [OnceBox](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/once_box.rs).
- [Rust condvar selection](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/condvar/mod.rs)
  and [Darwin relative wait handling](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/pal/unix/sync/condvar.rs).
- [Apple libpthread mutex](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/1f4f5265b319111142f1bf3a27d4484ef5a98314/src/pthread_mutex.c)
  and [condvar](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/1f4f5265b319111142f1bf3a27d4484ef5a98314/src/pthread_cond.c),
  reference release539.100.4, not proof of deployed dylib identity.
- [Already-pinned triomphe fallible shared allocation](https://raw.githubusercontent.com/Manishearth/triomphe/1c3cfa221b3f97729cbc989fc67976f45f874331/src/arc.rs).
  Passing it a live Child would consume ownership on refusal; this change never
  does that. No dependency or crypto configuration changed.

The identity record gives each fetched source's complete SHA-256, URL, revision,
retrieval timestamp and retrieval failures. SDK sizes are compile/layout facts,
not runtime capacity measurements. The existing parking_lot, atomic-wait and
Parker alternatives do not provide the required fallible cold timed-guard
contract without additional mechanisms; no new generic framework was selected.

One private interface serves the existing two mutex/CV pairs. Darwin uses the
tested native backend with fallible stable storage and checked native init,
lock, wait and notify results. Linux/Windows retain std's selected inline futex
backend and poison recovery. That adapter is shared across their architectures
and Linux libcs; it is not evidence that those targets passed this revision.

`Shared::prepare` builds both pairs and fallible shared storage BEFORE spawn.
No fallible synchronization constructor owns a live Child. Rejected native
mutex construction returns its exact input value. Only successfully initialized
native resources are destroyed; failed destruction retains storage. The rare
unpublished native cleanup failure still has no exposed retry owner; the
preserved constructor-owner draft is not silently applied or claimed tested.

State remains the sole guest terminal authority. Tail retains its existing
drain predicate and pipe-reader ownership. `changed` pairs only with State;
`drained` pairs only with Tail. Tail is released before State; the normal held
order remains State -> Child. Waits retain their original predicates/deadlines
and atomic unlock/wait/reacquire behavior. The Darwin duration cap derives only
from the native u64 nanosecond representation, not a replacement timeout policy.

Native synchronization failures carry typed operation/code/cleanup code through
`Refusal::Synchronization`; allocation refusal uses `Refusal::Allocation`.
Background observation failure is retained in State, without inventing a guest
exit. Native mutex acquisition/unlock failures are permanently retained in that
mutex, preventing a subsequent caller from entering its old State after an
exceptional independent reap. Guard Drop retains unlock failure for the next
acquisition; it does not manufacture a successful guard or panic.

The same Child Arc is now retained on Unix for exceptional cleanup. Stop/drop
attempt actual owned kill even if State access fails; the reaper still attempts
owned wait on its State-lock failure path. This is the existing Child owner,
not another lifecycle manager. In-flight native failure races, failure-latch
behavior and all native-error cleanup paths require ROOT's focused fault review
and runs. Ordinary success does not certify them or global panic/leak freedom.

## Actual bounded native results

All execution used ARM64 Mac `d1323898b255a3ed435eb83fa0f7b0cf`, `sync:false`,
admission1CPU/1GiB/2GiB, `-j1`, codegen1/LLVMthreads1, compile180s/case15s/
outer600s. Memory/disk admission is not an enforced Darwin RSS or aggregate
disk quota. Actual physical free space exceeded32GiB before each build.
Rust1.98.1 commit `48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8,
Darwin25.4.0 arm64; exact lock closure includes libc0.2.189 and triomphe0.1.17.

Commands are retained with arguments, environment and source identities:

```text
python3 binding_prepare.py --out NEW_SMALL_TRANSFER_DIRECTORY
python3 binding_run.py --bundle INPUT.json --root NEW_PRIVATE_SOURCE_ROOT --case-seconds 15 --compile-seconds 180
cargo +1.98.1 test --locked -j1 --profile test-release -p shards-api --lib --no-run --message-format=json
cargo +1.98.1 clippy --locked -j1 --profile test-release -p shards-api --lib --tests -- -D warnings
```

The closure exports the actual API/ABI/afunix/testdir source and pinned dependency
closure. Only workspace membership/irrelevant patch paths, lock membership and
test registration are adjusted privately. Production overlays are exactly the
three owned API files. Their bytes and the harness/driver are separately hashed.
No actual VM, network service, OOM pressure or profiling workload runs.

| Run | Actual outcome |
| --- | --- |
| Current main33df, r8 | Compile0/select1; ordinary and diagnostic cold connect each SIGABRT(-6), first64B; no timeout. Owned test/stand-in exits observed; after-abort grandchild reaping remains unknown. |
| First binding, r29 | Compile0; both cold cases passed1 each with typed allocation refusal and stop/wait. Ordinary stop passed1. Collector then failed on EPERM signalling the completed held test group; remaining ordinary cases did not run. |
| Bindingv2, r49 | Compile0; two cold + five ordinary cases passed, each exact1/no timeout. Strict Clippy101 for a safety comment attached to the enclosing expression rather than its unsafe block. |
| Final bindingv3, r54 | Compile0; both cold cases passed1 each, five ordinary cases passed1 each; strict native API library/tests Clippy0. Only comment/doc placement changed from v2. |

Final five ordinary cases cover stop, drop/reap, identity, owning patience0 and
owning patienceMAX. Each cold case asserts typed allocation refusal, then actual
stop/wait `Some(Stopped)`, then the actual parent's `waitpid(WNOHANG)=-1/ECHILD`.
Independent outer kqueue observers saw test and stand-in exits; the private
socket directories were absent afterward. First refused allocation is now32B
at the existing fallible `Socket::new`, not a fatal mutex allocation. No allocation
size is whitelisted. These samples are correctness evidence, not allocation or
latency profiles. The five ordinary cases have no independent descendant census.

All completed-group kill attempts' EPERM codes remain recorded. The v1 collector
error is retained; v2/v3 record the OS error and still explicitly wait/reap their
held test process. No security setting or signal workaround was attempted. This
does not establish successful group termination or universal descendant cleanup.

Final native test binary SHA-256:
`ecaaaec8e48b7e37f8c73cd4fd086acee907d1c4ec7843101472cb15f76bd79b`.
Production/test source hashes and commands are in
[input identities](binding-evidence/input-v3-identities.json) and the archived
`summary.json`/`clippy-result.json`. Native artifacts remain in
`/Users/adalundhe/managed-cold-sync-binding-v1-1luhy_9i/v3/execution/binding-33df-v1`.
Earlier source, binaries, failures and raw files remain preserved separately.

[Archive index](binding-evidence/archives.json) identifies four byte-preserving
text-encoded archives. All archive hashes, regular safe paths and original
member sizes/hashes were verified locally. Raw libtest blank EOFs/aligned traces
and derived-lock bytes stay compressed, without normalization or whitespace
waivers. Derived local originals are retained at
`/Users/adalundhe/managed-cold-sync-binding-raw-retained`.

## R01–R16 and owed work

| Rule | Actual evidence / remaining limit |
| --- | --- |
| R01 | Current fatal cold allocation reproduced, then typed actual API refusal and owned reap passed. Native init/lock/wait results checked. Existing startup/thread/String/Child-Arc allocations, native failure cleanup and all concurrent exceptional paths remain unverified; no global panic-free claim. |
| R02 | Extensive pinned alternatives plus actual current-source red and standalone notification mutation/restoration precede binding; ROOT approved this scoped native primitive. Final combined choice/cost review remains ROOT-owned. |
| R03 | Storage derives from actual Layout; duration cap is native representability. No copied64/page/default/capacity or benchmark-derived runtime number. Fixture budgets are explicit caller/test inputs. |
| R04 | Existing State/Tail predicates, Child identity and lock order preserved; native causes and observer uncertainty typed. Error latches contain native failures, not mirrored lifecycle phases. Exceptional in-flight races owed. |
| R05 | Two actual existing pairs justify the private interface. No warmer, cache, polling/spin fallback, parallel owner or manager. Existing connect pacing is unchanged. |
| R06 | Focused constructor/control/wait changes; no unrelated repository audit/refactor. Root reviews exceptional-error complexity. |
| R07 | Safety/pairing/ownership/native representability comments retained; strict Clippy0 on final Mac closure. |
| R08 | No comparable baseline, distributions or4–5x result claimed; all performance gates owed to ROOT. |
| R09 | Actual exact main33df red captured before production wiring; historical122/f39 results alone were not used as current red. |
| R10 | Current red and exact candidate green/raw commands/binaries/cleanup retained. Bound-API guard-removal/restored proof remains the next scoped correctness item; prior backend wake mutation is linked separately. |
| R11 | ARM64Mac actual seven focused cases/Clippy only. All seven-target and other native-host gates unrun here; Intel Mac unsupported, paused WinARM untouched. |
| R12 | One shared caller contract, smallest necessary Darwin pthread adapter established by cold red; Linux/Windows preserve std backend. No extra installed binary, privilege or crypto change. |
| R13 | Existing State/Tail/Child owners remain; no public mutex framework. Typed API variant requires exhaustive downstream review. |
| R14 | No profiles/benchmarks/cost/fault counters. Parent/kernel/C-runtime allocations, contention and resource-return distributions remain ROOT-only. |
| R15 | Sole writer; no children/goals/planning wrappers/main merge. Exact private admitted source/target and finite bounds; signed checkpoint is not integration acceptance or lane release. |
| R16 | No local compile/target/cache deletion/full worktree. Remote own small closure only; all originals/frozen/root caches/paused VM preserved. No cleanup needed or claimed. |
