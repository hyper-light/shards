# Ordinary final-owner teardown: source preparation

Prepared for ROOT review under seq7071. **No build, typecheck, native test or
runner was started.** There is still no reproduced valid destroy failure or
leak. This adds test observations to the existing candidate; it does not correct
production cleanup or resolve the separate forgotten-guard contract.

POLICY_READ implementer, current origin/main
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`, full AGENTS/R01–R16/evidence/CLAUDE/
managed plan and updated `/Users/adalundhe/g121-handoff.md` reread. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Sole writer; owned paths are the cfg(test) synchronization/worker observation
spans in `crates/api/src/{sync.rs,client.rs}` and this new directory.

Base is signed `d1e9fd94cddebb82265c50b9561a8692a40b4621`; its API bytes are the
frozen ec494/f08 candidate, not a native primitive adopted on main0342. All prior
source, raw failures, frozen binaries and holds remain preserved. In particular,
r124's fixture-diagnostic Clippy101 and r125's exact corrected ordinary passes
remain separate historical receipts; neither is rerun or inherited here.

## Exact hooks and caller

[hooks.diff](hooks.diff) is the exact diff of the two API files against d1e9.
[caller.diff](caller.diff) compares the new [case.rs](case.rs) with the frozen
ordinary `concurrent_case.rs`; the original file is untouched.
[source-identities.json](source-identities.json) pins their complete byte hashes.

| Prepared span | Observation and ownership |
| --- | --- |
| Darwin Mutex/Condvar | A cfg(test) OnceLock stores one instance-bound Watch containing only an object tag and Sender. It owns no Guest, Shared, guard or native pointee. Attachment happens while the returned Guest already owns Shared. |
| Existing native Drop | Emit DestroyEntered; call the original pthread destroy exactly once; emit the actual integer DestroyReturned. Keep the original code==0 release branch unchanged. Emit PointeeDropReturned only after its existing ManuallyDrop::drop returns; otherwise emit PointeeRetained. |
| Shared | cfg(test) Drop entry emits SharedDropEntered. A cfg(test) last field emits OriginalSharedFieldsDropped after the original state/changed/tail/drained/end_mark fields finish their Rust drop glue. The observer field's own drop and the enclosing allocation are still distinct. |
| start_threads / Guest | Apple test builds retain the actual existing drain/watch JoinHandles in Guest until the fixture takes them. No new worker is spawned. Taken-back guests have no invented handles and are outside this owned-child case. |
| Ordinary caller | Derive only AnswerThenStop from the existing stand-in/muxer fixture. Keep owning ZERO/MAX, two public wait_for callers, stop, actual ECHILD and six existing fixture joins; add the two actual API worker joins, final Guest drop and event reconciliation. |

Outside Apple test builds, StartedThreads is an alias for unit and both handles
are explicitly dropped at their original discard points. This is the small
source glue needed to expose the handles in test builds; **non-test source bytes
are not identical**. No native cleanup call, native result, error branch, guest
State transition, public signature, borrowed connector span, wait or notification
algorithm is changed. Compiler/lint and ROOT source review of that equivalence
are pending; a textual diff is not a runtime gate.

The observer is [observer.rs](observer.rs), included only for Apple cfg(test).
It is a finite, per-instance typed witness, without a registry, logger, callback
framework, new manager, cache or lifecycle mirror. No global allocator hook or
native-error injection exists. OnceLock only attaches test metadata once; it
does not initialize or warm the native object. Channels may allocate and affect
scheduling. This instrumentation is not observationally free or usable as a cost
or allocation-count measurement.

## Proposed live order — execution remains held

1. Admit a NEW private ARM64 Mac source/cache/target, existing lockf coordination,
   free disk at least30GiB; 2CPU/2GiB/4GiB, j2, outer at most600s. Keep all ROOT
   targets and paused WinARM untouched. Compile at most180s; one ordinary case at
   most15s using the existing held-exit supervisor. These are inherited scope
   ceilings, not new runtime defaults. No work has been dispatched.
2. Compile the exact reviewed source and pin the resulting executable. Select
   exactly `client::destroy_observation::ordinary_final_owner_releases_native_storage`.
   This case is explicitly `#[ignore]` because it requires the owned stand-in,
   environment/deadline and supervisor. Its declaration is not a recorded ignore
   count or passing skip.
3. Start the existing actual owned-child stand-in through `client::start`.
   Bind four watches to that real Shared's State/changed/Tail/drained, plus its
   last-field witness. Drop the fixture's Sender. Take the two actual internal
   JoinHandles from Guest without cloning Shared. Then share Guest with the
   ordinary public caller threads.
4. Preserve the original private socket marker/create_new, COLD_READY and outer
   observer-before-release handshake. The controller must hold/reap the actual
   test process and observe the stand-in exit. No pre-existing path is removed.
5. Read both actual CONNECT requests before answering; require both owning
   ZERO/MAX calls and both public wait_for calls still pending. Answer both;
   require both streams succeed. Stop through the same Guest; both waiters must
   return Some(Stopped).
6. Join two connectors, two public waiters, stopper and muxer; close the owned
   listener; retain the existing no-extra-dial observation and actual parent
   waitpid=-1/ECHILD. Then join the actual API drainer and watcher. The watcher
   performs the real capture/reap/notification path. No taken-back watcher or
   detached-thread census is inferred.
7. While the remaining Guest still owns Shared, require the observation channel
   empty. Recover its unique public Arc with try_unwrap; drop that actual Guest.
   Require the typed observations below and channel disconnection before the
   **same** fixture deadline. No observation message retains an owner.
8. The existing held outer supervisor must reconcile actual test/child exits and
   private directory absence. Preserve exact stdout/stderr/status/source/binary
   identities and any failure. No cleanup is silently retried or called passing.

The fixture uses one absolute Instant for its own readiness/channel checks;
owning ZERO/MAX patience is unchanged. JoinHandle::join has no timeout; checks
after join cannot bound a hang. The original finite outer held-exit supervisor
is therefore mandatory, and its deadline is never extended. The prior runner's
case argv does not include `--ignored`: a future reviewed private copy must add
that flag for this exact case and retain selector/count validation. No old runner
has been edited, dispatched or relabelled compatible.

## What each observation would prove

Expected successful source order is two Shared points plus three points for each
of the four actual native objects, fourteen events derived from the concrete
field graph:

```text
SharedDropEntered
State:   DestroyEntered -> DestroyReturned(raw code) -> PointeeDropReturned/Retained
Changed: DestroyEntered -> DestroyReturned(raw code) -> PointeeDropReturned/Retained
Tail:    DestroyEntered -> DestroyReturned(raw code) -> PointeeDropReturned/Retained
Drained: DestroyEntered -> DestroyReturned(raw code) -> PointeeDropReturned/Retained
OriginalSharedFieldsDropped
observer Sender ownership ends -> receiver disconnection
```

The case records every received raw return/event before comparing with its
ordinary-success expectation (four zero returns and four PointeeDropReturned).
An unexpected native code is preserved and fails that assertion; it is not
treated as a native pass. The existing retention branch still retains its actual
pointee/payload resources. No retry, free-on-error or abort policy is introduced.

PointeeDropReturned means the existing Box drop/deallocation path returned. It
does not mean the allocator returned pages to the OS, nor prove all process/FD/
kernel resources returned. OriginalSharedFieldsDropped covers original fields,
including end_mark, but excludes the observer field's own remaining drop glue.
Shared's enclosing triomphe allocation is deallocated later; **there is no hook
for that deallocation here**. Sender disconnection is a channel ownership fact,
not an allocator callback. Do not infer either fact from native return0.

Ordinary quiescence relies on real caller/worker completion, their guard release,
the Guest/Shared owner graph and final-owner Drop entry. A test-thread entry
announcement still does not prove a native CV park, and no wait-park/duration
claim is added. The joins and final-owner point must be observed live before
calling even this bounded ordinary obligation passed.

Preparation/constructor rollback happens before observer attachment and is not
covered. Native init/lock/wait/unlock/notify errors, destroy refusal/retention,
forgotten guards, foreign cancellation and exceptional stale-State/pendingCancel
paths remain open. This source preparation authorizes none of those actors.

The language specifies Drop before field drop and struct fields in declaration
order; joining waits for thread termination and synchronizes its operations with
the joiner. [Rust Reference](https://doc.rust-lang.org/reference/destructors.html),
[pinned Rust join source](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/thread/join_handle.rs).
[primary-identities.json](primary-identities.json) pins the compiler's Reference
submodule/destructors source and join implementation. Unversioned std docs render
1.99.1 and are not substituted for the actual Rust1.98.1 source. Native contracts
and their deployment limits remain in [destroy-contract](../destroy-contract/README.md).

## Checks, unknowns and R01–R16

Local parse/format, whitespace and development-policy checks are recorded in
[checks.json](checks.json). **Compilation, strict lint, test selection, ordinary
native outcome, raw destroy codes and cleanup outcomes are all UNRUN.** No ignore/
skip/pass count exists. First source patch used the wrong public-struct anchor
and was rejected without edits. Two source-fetch declaration checks looked in
thread/mod.rs before resolving its actual join_handle module; failures are
recorded rather than reported as source/runtime failures.

| Rule | Evidence / remaining obligation |
| --- | --- |
| R01 | No production cleanup or panic policy changed; test assertions only. Current generic forgotten-guard/safety-comment and exceptional native errors remain open. No new unsafe operation. |
| R02 | d1e9 pinned POSIX/Apple/Rust/established alternatives plus pinned field-order/join sources. This prepares its smallest legal ordinary proof, not a replacement architecture. |
| R03 | No production number; four object tags, two Shared points and concrete caller/worker counts derive the event bound. Existing protocol/fixture inputs retained. |
| R04 | Sole production State/Tail/Shared, no parallel phase ledger. Observation tags distinguish native operation, pointee fate and original field-drop point. |
| R05 | Instance-bound test witnesses and existing notifications; no cache, warmer, polling/spin, logger, manager or retry owner. Native CV-park coverage unknown. |
| R06 | Only native destructors, real handle ownership and the existing ordinary caller seam; no audit or broader rewrite. |
| R07 | Observation points and limits explicit; existing disputed production comments preserved pending ROOT decision. |
| R08 | No cost/4–5x claim; instrumentation costs unmeasured. |
| R09 | No valid destroy failure or leak reproduced; no speculative production fix. |
| R10 | Exact base/diffs/source hashes, failed preparation steps and unrun gates retained; old r124/r125 and all prior source/raw preserved. |
| R11 | Apple cfg(test) only; no new native or seven-target pass, VM or Intel Mac claim. Paused WinARM untouched. |
| R12 | Existing shared owner/native adapter only; no OS mechanism, deliverable, host/root/security/crypto change. |
| R13 | One ordinary case, existing workers and bounded instance messages; no enclosing allocator hook or retry interface expansion. |
| R14 | No profiler/benchmark/fault/allocator actor or allocation-count/leak-free claim. Enclosing Arc allocation, physical/kernel return and error retention remain unknown. |
| R15 | Sole writer, no children/goals/main merge. ROOT reviews exact source before any execution and owns combined/VM/fault/cost gates. Checkpoint is not lane release. |
| R16 | Local source/small docs/format only; no build/runner/target/worktree/copy/cleanup. Existing source/data/frozen evidence/protected caches preserved. |
