# MR-OOM-MAC-1: pinned native pthread mechanism probe

TEST ONLY, not a production synchronization implementation or selection. New
`p/managed-sync-probe` from immutable05f026690; implementer owns only this
prototypes directory. Full policy read: canonical g121-main AGENTS/R01–R16,
evidence/CLAUDE/managed plan/handoff atf5cec365d2e435afae5e6f613c92d9d3759ef60f,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root owns alternatives/selection, audits, all profiles/benchmarks/heavy campaigns,
combined gates and merge. No production API/std synchronization, reader, lifecycle,
crypto, VM or installed binary changes. The original Mac owned-Guest/std OnceBox
allocation abort remains open; this probe does not warm or skip it.

## Mechanism and safety boundary

A standalone Darwin arm64 Cargo executable pins libc0.2.189, exactly the version
and checksum already in the workspace lock. Its nested workspace/lock do not
change the production dependency graph. The sole opaque mutex/condition pair
lives in a fixed Storage allocation obtained through Global alloc/null check,
initialized as Rust MaybeUninit/UnsafeCell storage, transferred to a matching
Box, then pinned **before** either native init. No initialized pthread object is
copied/moved, including during partial initialization/destruction.

The main thread owns phase Empty→Mutex→Pair and destroys Pair→Mutex→Empty. One
native pthread exchanges Waiting→Released→Done events through the associated
native mutex/condition, then is joined before destruction/deallocation. There
are no Rust std Mutex/Condvar, shared refcount, generic sync interface, cache,
poll loop or production wrapper. Native errors retain a typed call+return code.
A failed join/unlock with potentially live worker or failed destruction retains
the allocation until process exit rather than freeing possibly-live objects;
that terminal path is explicit and unproven here. No pthread cancellation is used.

The artifact requires a positive outer-wait nanosecond input; there is no default.
The launcher derives the existing cold conformance PATIENCE Mac bound (twice its
recorded maximum), not a new runtime constant or performance assertion. Both
threads use that one absolute CLOCK_REALTIME deadline and predicate loops for
legal spurious wakes. Wall-clock changes/native unexpected failure liveness are
not independently tested; kiln's outer job bound also contains a failed probe.

An expired Unix-epoch timespec independently exercises the actual native
ETIMEDOUT error and mutex reacquisition before unlock. It does not fabricate an
initialization failure or call APIs with invalid/uninitialized attributes.
Native init failure return paths are handled but not demonstrated unless a safe
injector actually produces them; none is currently installed.

## Allocation injection means only this

The artifact reuses the existing committed managed-borrowed allocation.rs
unchanged. It refuses Rust Global allocations on the **calling thread** only.
One case refuses the initial fixed Storage allocation, which must return typed
Allocation before native init. A separate fresh invocation first obtains pinned
storage, then performs native init/lock/timed wait/signal/thread join/destroy
while Rust Global allocation is refused on the main thread. No logging is done
inside that refusal interval. The native worker has no explicit Rust allocating
operation, but the thread-local injector is not proof about every worker/runtime
allocation. In particular it does **not** intercept libpthread/libc malloc,
native internal allocation failures or kernel charges. No native-allocation-free,
cost, leak-free or production panic-free claim follows from success.

## Primary interface references

[Apple's pthread_cond_timedwait manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/pthread_cond_timedwait.3.html)
documents associated-mutex release/reacquisition and explicit timeout/error
returns. [Apple's synchronization example](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/ThreadSafety/ThreadSafety.html)
uses the same pthread mutex/condition predicate pattern. The checked API/source
reference is Apple libpthread commit42d026df5b07825070f60134b980a1ec2552dfee:
[header](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/42d026df5b07825070f60134b980a1ec2552dfee/include/pthread/pthread.h),
[mutex](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/42d026df5b07825070f60134b980a1ec2552dfee/src/pthread_mutex.c),
[condition](https://raw.githubusercontent.com/apple-oss-distributions/libpthread/42d026df5b07825070f60134b980a1ec2552dfee/src/pthread_cond.c).
Those inspected sources are not claimed byte-identical to the installed OS
library. Runtime OS/compiler/SDK identity and header hash are retained by the
launcher. Root owns the broader established-alternative research and decision.

## Light checks and owed work

Native d132 run r210 completed exit0 at exact clean source
`bce20df21335db49f03cbbe9386e0b925789845b`. It was admitted once at4CPU/8GiB/20GiB
with29/32 cores idle; actual df remained above30GiB. macOS26.4 build25E246,
Darwin25.4.0 arm64, Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8. Exact launcher:

```sh
lockf /tmp/g121-mac.lock bash docs/research/measurements/managed-sync-coordinator/prototypes/run-light.sh
```

- Fresh standalone release build0; selected artifact `fresh:false`.
- Separate fresh `allocation-refused` invocation0:
  `pinned_storage=Allocation native_init_not_entered=true`.
- Separate fresh `native-under-rust-refusal` invocation0:
  `native=Ok(()) expired_wait_typed=Some(Pthread { call: Wait, code: 60 }) cleanup=Ok(()) retained_unknown=false`.
  Thus checked init/lock/wait/signal/join/destroy completed and the expired wait
  returned actual Darwin ETIMEDOUT60. No native initialization failure was injected
  or proven, and native malloc/kernel behavior was not observed by this injector.
- Strict artifact all-target clippy0; mechanical rustfmt/bash/policy checks0.

Private copied source lives at
`~/managed-pthread-probe-check/docs/research/measurements/managed-sync-coordinator/prototypes`;
binary `~/managed-pthread-probe-target/release/darwin-pthread-probe`, SHA256
`3103771dc1f431b5ff97d84e1710fa311e118a19faa79d34d0b266212ad12616`.
SDK `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`; opaque pthread type
header SHA256 `741433612525badf767d48b1924d4c7ba564ccc5c1bab5cb7045c238850fa86b`.
Existing cold PATIENCE input was268323834ns; it was a liveness budget only.
[raw-d132](raw-d132/) retains exact commands/logs/compiler/SDK/source/binary
manifests and admission/final status. Archive SHA256
`36f100aafb979ec45df13ea37f0ee9e2f21965da7101100dcf2c8b3700732fc9`.

Before completion the API repeatedly reported running/empty output while the
script evidence directory was absent. A scoped read proved selected exact source
files had arrived. An initial possible-lock interpretation was withdrawn; no
lock cause was established. The accepted job/lock/source were preserved and no
duplicate writer was launched by this agent. Eventually actual native logs were
returned; the earlier delay remains unattributed, not a startup/performance
measurement. [setup-observation.txt](raw-d132/setup-observation.txt) records this.
Root separately authorized a coordinator-owned unchanged-source b24 check with
distinct prefixes; its results are not counted here.

Local Mac builds remained held near31GiB; b24 root clone/target were untouched.
No sweep/exhaustion/cost measurement or production synchronization choice occurred.

R01/R04/R06/R07/R13: test-only pinned storage, explicit init/exchange/destroy/error
states and comments. R02/R03/R05/R12: native interface evidence, no architecture
selection/new production default/poll/cache/framework. R08/R14: no cost/speed
claim; all profiles/benchmarks remain root's. R09/R10: the existing actual Mac
abort is unchanged, this is a mechanism experiment with retained commands/raw,
not a fix. R11: Darwin arm64 only, not seven-target/VM coverage. R15/R16:
implementer light scope, no children/main merge, private admitted runner artifacts,
no cache cleanup or local compile. Owed root: native init-failure injection and
partial cleanup under such failure, contention/spurious wake/cancel/owner loss,
resource/latency/CPU/allocation/fault costs, established alternatives review,
actual caller integration/independent mutation and all combined platform gates.
