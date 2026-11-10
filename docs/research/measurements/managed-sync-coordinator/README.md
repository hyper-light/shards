# MR-OOM-MAC-1: actual owned-connect mutex allocation abort

**Open production defect, no remedy adopted.** Coordinator receipt c921a0b1,
rules SHA2561ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d.
This is distinct from the new borrowed connector's successful focused checks.
No warmed/skipped test replaces the failing full-library gate.

## Independent actual reproduction

On macOS arm64/Darwin25.4, pinned Rust1.98.1/LLVM22.1.8, the coordinator rebuilt
clean API source122332036eb689c33fd52b4a065c53c7dc5e4530 into a separate private
target. The exact `client::tests::owned_connection_oom_is_typed_before_error_formatting`
test aborts with allocation64B failure, SIGABRT and cargo101. The first
`raw/owned-trace.log` is actually this uninstrumented baseline: the attempted
diagnostic edit stopped at a count assertion before changing source, and the
ordinary test then ran. That harness mistake is not a new product failure mode.

A second diagnostic changes only the test allocator, not production code or the
allocation refusal. At the first refused64-byte request it emits a fixed32-frame
C backtrace to fd2, then still returns null. Actual frames are:

```
std::sys::sync::OnceBox<pal::unix::sync::mutex::Mutex>::initialize
ShardsGuest::connect
client::tests::owned_connection_oom_is_typed_before_error_formatting
```

The process then aborts again with SIGABRT/cargo101. Exact patch, command, log,
source hashes and byte-identical source-restoration hash are in raw/owned-trace-v2.*.
This identifies the actual caller, beyond matching an allocation size. The
diagnostic unwinder may itself incur C/runtime costs; it is neither production
instrumentation nor an allocation/performance benchmark.

Separately, cold-mutex.rs constructs a distinct std Mutex after initializing
reporting, and counts Rust Global allocations only around its first lock:

| Mode / explicit injection-size input64B | Actual result |
| --- | --- |
| observe | one allocation,64B; exit0 |
| cold with null64B | fatal allocation failure; shell exit70 |
| warm-control | zero allocations; exit0 |

Warming is only a negative attribution control. It is not a safe remedy or a
passing replacement full-library test. The standalone exit70 is not relabelled
SIGABRT; the API cargo log independently reports SIGABRT. Initial rustc option
typo and corrected build log are retained. An LLDB attempt stopped before a
running test/trace; root cancelled only its own inferior/debugger. Its partial
log is retained, not counted as successful attribution.

## Primary source and scope

[Rust1.98.1 pthread Mutex::get](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/sys/sync/mutex/pthread.rs)
lazily initializes a pinned native mutex in OnceBox.
[The native wrapper](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/sys/pal/unix/sync/mutex.rs)
contains pthread_mutex_t. The local LP64 Darwin SDK's `_pthread_types.h` declares
56 opaque bytes plus one8-byte long, matching the actual64B request.
The original owned Guest has a cold std Mutex before its fallible channel path;
the borrowed path creates no Guest/watch thread/std Mutex.

A change to synchronization needs separate primary alternatives, contention and
condition-variable compatibility research, fallible initialization/error handling
and actual failure/cleanup proof. Replacing it with a guessed spinlock, warming
it or moving the same infallible allocation earlier is not approved. Other
existing Guest/simulator allocations are not certified panic-free by this proof.
This defect prevents a whole-library panic-free/readiness claim.

## Replay and artifact ownership

Run cold-mutex.rs with pinned rustc using `--edition=2024 -C opt-level=2`, then
invoke `probe observe 64`, `probe cold 64`, `probe warm-control 64` under
lockf /tmp/g121-mac.lock and a zero core-file limit. Exact source/host/compiler
identity and source hash are in raw/identity. The API command is in
raw/owned-trace-v2.command.json; apply the retained test-only patch only in a
private source/target, preserve its expected failing exit and restore the source.
Never use that instrumented artifact as an ordinary passing build.

trace-artifacts.json records both binaries. The original agent Mac artifact
remains unchanged at SHA25646054f6fa67730987529b54eb73b9a6bb0ca8f119828c5c9b88fddb312dade9d.
The root diagnostic binary is SHA25653ff2cc01648fb25443beb4356c06ca4cea7ae8134e7563cf2b158d90c171da8;
it remains instrumented despite source restoration. Root used a different
worktree and target. No source, credentials or other agent's work was removed.

R01: actual fatal-allocation violation remains open. R02/R03: observed stack,
pinned source/layout and explicit diagnostic inputs; no chosen runtime cap.
R04/R05/R06/R07/R12/R13: no production state/interface/synchronization redesign,
cache, polling mechanism or platform duplicate adopted. R08/R14: attribution
only, not total allocator/C/kernel costs or performance/4–5× proof. R09/R10:
independent real red, controlled source diagnostic and raw restoration records;
fixed green/mutation of a remedy remains owed. R11: native Mac arm64 only, no
Intel Mac or other-platform conclusion. R15/R16: coordinator diagnostics under
lockf, measured free space above30GiB, private artifacts/evidence preserved and
no paused VM or implementing agent interrupted.
