# Darwin State/Tail destruction and retention contract

**Proposed ROOT choice:** keep the existing Shared owner and its two native
pairs. Prove quiescence before destruction; do not use a destroy return as that
proof. No legal current-caller path to destroy refusal was established, and no
destroy failure or leak was reproduced. A retry manager or adoption of the
preserved constructor-owner draft is not justified by this research.

There is a separate safe-helper obligation: unique ownership does not imply
that a returned guard was unlocked. If ROOT requires the private generic mutex
to tolerate a forgotten guard, the smallest established candidate is Rust's
single terminal try-lock/unlock check **before** destruction, retaining storage
on refusal. This is a proposed subsequent contract/proof choice, not an
implemented fix or an authorized native actor.

## Policy, identities and limits

POLICY_READ implementer, canonical main
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`; full AGENTS, R01–R16, development
evidence, CLAUDE, managed plan and handoff read. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Initial handoff SHA `2caf9429d55574abd983717588eb76460db12a2dfd9ba7ba2e8c6bfa40df775f`;
after its update, full handoff reread at
`91bdbe09542049712c1b3c24665be1410ed5f301cca30bc1898e84327645e1fb`.
Own only this new `destroy-contract/` directory. The research skill's primary-source,
single-Markdown workflow is applied directly under the explicit no-children scope.

The concrete owner is the **unmerged candidate** at
`ec494ad24905e2f39fe05f77a9513a8fd41df5e9`, with f08 product bytes. Main0342 has
no `api/src/sync.rs`; its client/lib differ. This is not a finding about a native
primitive already adopted on main. [sources.json](sources.json) records both
revisions, full source/SDK hashes, twenty primary-source identities and retrieval
times/limits.

| Candidate file | SHA256 |
| --- | --- |
| `crates/api/src/sync.rs` | `303be4efd0ce83b54fbe7ae36cf7f7202d0f791bc7126e8aac533ff202026e4c` |
| `crates/api/src/client.rs` | `3941532a6ab8c450ba532001b19dca657a6327391c04c3666ee69b3635ad9a32` |
| `crates/api/src/lib.rs` | `b8d330ac62c6eda930bb581d2a925285d8d33ad3bdd60d83cfd0dfa3fb187e01` |

Reference pins are Apple libpthread539.100.4
`1f4f5265b319111142f1bf3a27d4484ef5a98314`, actual Rust1.98.1 compiler
`48a229ceaefd4985c50990b14116b6d856af0985`, triomphe0.1.17
`1c3cfa221b3f97729cbc989fc67976f45f874331`, LLVM22.1.8
`ca7933e47d3a3451d81e72ac174dcb5aa28b59d1`, libuv1.51.0
`5152db2cbfeb5582e9c27c5ea1dba2cd9e10759b`, and POSIX Issue8/2024.
No package/backend adoption follows from this comparison.

Local read-only context: ARM64 macOS26.4.1/25E253, installed CLT SDK26.4.
Its `libsystem_pthread.tbd` advertises539.100.4; its types header exactly matches
the pinned Apple types. The full pthread header differs in unrelated availability
annotations; relevant synchronization declarations/constants match. These are
ABI/link-source facts, not deployed dylib binary identity. Earlier d132 native
receipts remain their own macOS26.4/25E246 results. No fresh runtime check ran.

## Destruction preconditions precede error handling

POSIX requires an initialized, unlocked mutex with no other thread locking it or
using it in a condition wait. Destroying a locked/referenced mutex is undefined
behavior. A condition variable requires no blocked waiters. EBUSY/EINVAL misuse
diagnostics are recommendations in informative rationale, not a portable retry
protocol. Resource errors listed there concern initialization; neither destroy
function may return EINTR. A destroyed object needs reinitialization before use.
Retaining bytes after an unexpected error does not establish safe native reuse.
[POSIX mutex](https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_destroy.html),
[POSIX condition variable](https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_destroy.html).

The native integer is the error number, not errno to re-read. No repeated destroy,
EINTR retry loop or free-on-error policy follows from these contracts. For the
actual NORMAL, non-robust mutex, recursive/robust/priority-ceiling errors are not
ordinary successful-owner unlock outcomes. POSIX still permits lock failures
such as deadlock detection; not every lock/wait/unlock return is proved impossible.
[Lock/unlock contract](https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutex_lock.html).

Condition waiting releases and reacquires its mutex. POSIX deferred cancellation
reacquires it before cancellation handlers; cancellation is not an ordinary
successful return. The candidate has no native thread-cancel call. Foreign
cancellation or abandoned guards are outside the normal owner argument, not an
authorized failure actor here.
[Wait/cancellation contract](https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_cond_wait.html).

## Apple implementation versus deployment

The pinned mutex destroy returns EINVAL for an unrecognized signature, EBUSY for
recognized busy state, and zero for recognized idle/static state, invalidating
the signature on success. It supplies no resource-exhaustion branch for a valid
quiescent mutex. This is source analysis, not a live return or a deployed binary
match. [Apple mutex source](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/1f4f5265b319111142f1bf3a27d4484ef5a98314/src/pthread_mutex.c).

The pinned condition destroy returns zero for pristine/ulock state. In its
psynch busy test, the EBUSY assignment is commented out and destruction continues.
Therefore candidate `Condvar::drop`'s comment that successful destruction proves
no active waiters is unsupported. Ownership must establish that premise first;
retention on nonzero cannot guard an invalid call that returns zero.
[Apple condition source](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/1f4f5265b319111142f1bf3a27d4484ef5a98314/src/pthread_cond.c).

The same reference initializes valid private attributes, accepts NORMAL, and
returns zero when destroying valid attributes. Its EINVAL branch requires a bad
signature. No producing branch for the constructor's attribute-destroy plus
mutex-destroy double failure was found under those reference invariants. POSIX
attribute resource failure concerns init, not a documented operational retry
case for valid destruction. This does not prove every deployed/future binary.
[Attribute contract](https://pubs.opengroup.org/onlinepubs/9799919799/functions/pthread_mutexattr_destroy.html).

## Concrete owner and quiescence argument

| Actual owner/path | Lifetime fact and limit |
| --- | --- |
| `Shared::prepare` | Builds both stable pairs before Child spawn or guard exposure. Earlier initialized fields remain owned during prefix rollback. Failed `ControlArc::try_new` drops its unpublished Shared argument. No live Child is consumed. |
| `ShardsGuest` | Holds a strong ControlArc. Public methods borrow the guest while their native calls/guards execute. Guest Drop requests stop; it does not synchronously join both internal workers. |
| drain closure | Holds its cloned ControlArc through drain/failed, Tail access/release and notification. |
| exit/taken closure | Holds its ControlArc through capture/State/wait/notify/release. A blocked worker retains Shared rather than authorizing destruction. Such a live owner is not evidence of native destroy failure. |
| final ControlArc release | Triomphe's release decrement/acquire fence destroys Shared after other strong owners release it. This excludes ordinary outstanding borrows/calls, not forgotten native acquisition state. |

The exact [triomphe source](https://raw.githubusercontent.com/Manishearth/triomphe/1c3cfa221b3f97729cbc989fc67976f45f874331/src/arc.rs)
supports final release and unpublished fallible allocation. State pairs only with
changed; Tail only with drained. Actual paths in failed, drain, capture, watch,
with_state, connect and wait_for release explicitly or via Guard Drop. Successful
same-thread native unlock is an additional premise. Native failure does not prove
that premise; exceptional paths remain open.

No production guard-forget/raw escape was found in these callers. Attach tests
forget the **guest**, retaining its Shared owner; they do not forget a native
guard and destroy its mutex. Public fixture threads joining and child ECHILD do
not prove that the last detached worker already dropped Shared or that native
destruction returned zero.

The argument is conditional on valid initialization, stable addresses, current
pairing, ordinary Rust owner retention, guard release and no foreign cancellation/
corruption. It establishes no legal current-caller destroy-error path without
breaking those premises. It is not exhaustive exceptional-path proof.

## Exactly what retention means

| Current branch | Source-level fate |
| --- | --- |
| Mutex Drop, native0 | Drops Box, native storage and T. |
| Mutex Drop, nonzero | Leaves the whole `Inner<T>` undropped through ManuallyDrop, including State strings or Tail deque/PipeReader if present. No recipient receives its pointer. |
| Condvar Drop, nonzero | Retains the native Box without an exposed retry owner. Zero alone is not a quiescence certificate. |
| Constructor attribute cleanup and mutex destroy both fail | Returns exact original T and Failure; T was never placed in the allocation. Retains raw native storage without an exposed owner. No actual sample exists. |

The current `new(T) -> Result<Mutex<T>, (T, Failure)>` preserves rejected
**payload** ownership, not every native allocation's ownership. A consuming close
interface would not prove unlocked state; final-Arc Drop also has no recipient
waiting for a returned cleanup owner/error. A new retry token does not complete
the actual published-owner path.

The distinction is visible in the existing interface:

```text
guard = mutex.lock()?        // borrows wrapper, owns native acquisition
forget(guard)               // Rust allows skipping its destructor
unique mutex value remains // uniqueness does not establish native unlock
```

This is an interface illustration, not a compiled/executed fixture or actor.
Rust requires unsafe implementations to tolerate forgotten returned values.
[Pinned forget contract](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/core/src/mem/mod.rs).
The safe generic helper cannot justify locked destruction solely from `&mut self`
or expected Guard Drop. That abstraction obligation is separate from the current
no-forget caller argument. An invalid native destroy is not a legal leak reproduction.

## Established choices

| Alternative | Handling and implication |
| --- | --- |
| Rust1.98.1 pthread mutex | Takes the native owner; tries locking once, unlocks on success, then permits PAL destruction. If locked, retains storage without calling destroy. Handles guard leakage before violating the native precondition; no polling loop. |
| Rust PAL mutex/condvar | Calls native destroy with debug assertions under caller preconditions. Not a Shards R01 error policy or recoverable-owner API. |
| libc++22.1.8 | Nontrivial destructor bodies call native destroy without an error recipient; target/ABI can select trivial destruction. C++ lifetime assumptions do not establish Rust guard-forget safety or permission to ignore errors here. |
| libuv1.51.0 | Aborts on destroy errors; Darwin condvar destruction first performs a timed wait with a temporary mutex for a historical compatibility concern. Neither abort nor copying that prewait without a matching current failure is acceptable. |
| Explicit close/retry owner | Requires a real recipient and known valid native state. Cannot create quiescence or a recipient after last Shared Drop. No legal failure currently justifies this expansion. |

[Rust wrapper](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/sync/mutex/pthread.rs),
[PAL mutex](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/pal/unix/sync/mutex.rs),
[PAL condvar](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/sys/pal/unix/sync/condvar.rs),
[libc++ mutex destructor](https://raw.githubusercontent.com/llvm/llvm-project/ca7933e47d3a3451d81e72ac174dcb5aa28b59d1/libcxx/src/mutex_destructor.cpp),
[condition destructor](https://raw.githubusercontent.com/llvm/llvm-project/ca7933e47d3a3451d81e72ac174dcb5aa28b59d1/libcxx/src/condition_variable_destructor.cpp),
[pthread adapter](https://raw.githubusercontent.com/llvm/llvm-project/ca7933e47d3a3451d81e72ac174dcb5aa28b59d1/libcxx/include/__thread/support/pthread.h),
[libuv](https://raw.githubusercontent.com/libuv/libuv/5152db2cbfeb5582e9c27c5ea1dba2cd9e10759b/src/unix/thread.c).

For ROOT, retain the owner and correct the precondition contract first. If safe
guard-leak tolerance is required, compare the small Rust-style terminal precheck
with a broader scoped-guard rewrite; the latter changes more actual callers.
Preserve nonpanic checked results/conservative retention. Do not copy assertions,
aborts, a new waiter ledger or historical prewait. No production change is selected
or implemented in this checkpoint.

## Existing evidence and next ROOT proof

[BINDING](../BINDING.md), [mutation/restoration](../MUTATION.md),
[concurrent callers](../CONCURRENT.md), [f08 Linux/Darwin](../linux/README.md)
and [Windows](../windows/README.md) remain frozen. None records native destroy
outcomes or complete destructor-resource return. Linux/Windows select the other
adapter. Their passes are not new Darwin destruction evidence.

ROOT's smallest subsequent proof should observe legal ordinary teardown of the
same owners: unused pairs, notified-but-never-waited CV, returned timeout/notify
waits, normal State/Tail terminal paths, guest Drop while workers still own Shared,
and actual final-owner destruction. A future approved finite test-only observer
may record existing destroy entry/return and final-owner point. It must not
manufacture return codes, destroy busy objects or infer cleanup from child reap.
This is a proposed next scope, not an executed gate or actor authorization.

If legal teardown returns nonzero, preserve exact source/binary/libpthread identity,
call sequence, native code and retained allocation/payload fate before changing
ownership. Valid observed0 results establish only their bounded ordinary cases;
exceptional retention and other deployed versions remain unknown. Guard-forget
resolution is a standards/safety review, not permission for UB, injected native
errors or a held allocator campaign. ROOT owns failure actors, costs and combined gates.

## R01–R16 account

| Rule | Evidence and remaining limit |
| --- | --- |
| R01 | Production read-only; no assertions/aborts substituted. Guard-forget/native preconditions and exceptional error behavior remain explicit obligations. |
| R02 | Twenty pinned primary sources and exact caller/SDK comparison. Alternatives/unknown deployment stated; subsequent choice/proof belongs to ROOT. |
| R03 | No runtime constant/capacity/default. SDK representation is not measured cost or sizing policy. |
| R04 | Existing Shared/State/Tail authority; no extra waiter/lifecycle ledger. Initialization, ownership, busy/unknown and final release distinguished. |
| R05 | Two existing pairs; no retry manager, generic framework, warming/cache/poll/spin. Terminal precheck remains an unimplemented alternative. |
| R06 | Focused constructors, guards, waits, destructors and callers, not repository audit. |
| R07 | Unsupported successful-destroy quiescence comment identified; frozen source untouched. |
| R08 | No performance result; comparisons remain ROOT-owned. |
| R09 | No destroy refusal/leak reproduced. Source branches, UB and old ordinary samples are not current legal failures. |
| R10 | URLs/revisions/full hashes, source comparison, retrieval failures and old evidence limits retained; no runtime pass claimed. |
| R11 | ARM64 Mac SDK research only; new native/VM/seven-target checks unrun, WinARM untouched. |
| R12 | Existing shared owner/adapter, no OS mechanism or deliverable. SDK/source is not deployed-binary proof. |
| R13 | Smallest proposed boundary before owner/retry expansion; old draft remains unapplied. |
| R14 | No profile/benchmark/allocator/native-error actor or leak-free/resource-return claim. Retained storage may retain payload/resources. |
| R15 | Sole writer/new directory only; signed named paths/attribution, no children/goals/main merge. ROOT reviews/assigns subsequent proof. |
| R16 | No cache/target/source deletion or worktree. All old source/raw/frozen artifacts/protected ROOT scopes preserved; no runner used. |

Only read-only source/network retrieval and local textual/identity/policy checks
ran. No interface typecheck was necessary; illustration explicitly unrun.
The sustained lane and P0–P13/G01–G25 remain open.
