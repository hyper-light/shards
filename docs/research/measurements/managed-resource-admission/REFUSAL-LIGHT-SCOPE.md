# Next bounded allocation-refusal check

Status: proposed to ROOT via key
`managed-next-cap-refusal-light-scope-request-v1`; no injector source or execution
authorized by that proposal alone. The completed production source72c and the
signed/pushed15-case checkpoint53a0897d7 are preserved. This would be a functional
light test of two actual cap-admission allocation paths, not a profile or a new
production allocator. ROOT may select a different actual-cap batch.

The current ninth unit fills the diagnostic writer's real String capacity and
checks refusal of a further fragment. It does not inject allocator failure.
The two next call sites are the name Vec reserve before `Group::create` can
mkdir, and the diagnostic String reserve in `Refusal::message` before rendering.

Use a new private single-process test artifact with one std System/GlobalAlloc
wrapper. A stack-controlled atomic gate would return null only during one of
those calls; successful operations and deallocations delegate to System with
the original layout. A failed realloc must leave the original block untouched.
The wrapper would use only atomics/pointers/System, with no formatting, locks,
callbacks, logging, allocation or unwinding in allocator methods. Do not call
the allocator with invented invalid/zero layouts, mix allocators, or inspect a
pointer after a successful realloc. These are the pinned contracts, rather than
a new OS-specific allocator mechanism. [Rust1.98.1 GlobalAlloc safety and methods](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/core/src/alloc/global.rs),
[official System wrapper example](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/alloc.rs).

The hook also needs a plain atomic refusal-event witness. Rust explicitly allows
allocation elision, so a case without an observed hook refusal cannot be called
an injected-allocation pass. Such a result must remain an explicit missing
witness/non-pass; no unsafe assumption may force an allocation. A fresh process
avoids parallel libtest fault windows; it does not establish universal runtime
quiescence or exclusive call-site/process-tree allocation coverage. The gate
would be disabled before checking outcomes or emitting records. No new Drop
logger/catch, allocator warmup, OOM handler or production registration is proposed.
[Pinned optimizer/re-entrance obligations](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/core/src/alloc/global.rs).

Expected checks, only if ROOT approves:

1. An observed allocator refusal yields typed Cause::Allocation from the actual
   name reserve, with no new private directory. Keep the owned parent and all
   record setup outside the fault window.
2. An observed diagnostic reserve refusal returns an empty String while the
   caller's result remains Err. Do not substitute an empty success, ignored
   formatting result or allocator-OOM claim based on the current capacity test.

String's fallible exact reserve delegates to Vec, which delegates to its raw
buffer and returns an allocation/capacity error. It may receive more capacity
than requested; this is why production checks actual remaining capacity.
[Pinned String implementation](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/alloc/src/string.rs),
[pinned Vec implementation](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/alloc/src/vec/mod.rs).

Alternatives have different proof scope: the existing capacity-fault unit
exercises bounded writes, and a deliberately excessive reserve exercises size
overflow. Neither observes allocator refusal at these two call sites. OS memory
exhaustion/limits would test a broader native boundary and remain ROOT-owned.
The System wrapper avoids introducing a private allocator dependency, new nightly
allocator API, native adapter or production resource policy.

If selected, use exact unchanged production body plus a documented private test
entry suffix, new own remote source/target/log paths, serial1CPU/1GiB RAM/2GiB disk,
finite command deadlines, no local compilation or root-cache writes. Preserve
exact commands/source/artifact identity, observed hook/outcomes, stdout/stderr and
all failures/unknowns. Any abort/nonzero/missing witness is retained. No actual
controller/VM/profiling/mutation campaign or guarantee is authorized here.

Research retrieval limit: unversioned rustdoc currently identifies1.99.1, whereas
the actual light compiler is1.98.1. Four versioned rustdoc URLs for GlobalAlloc,
System, String and Vec were inaccessible through the web tool; the four exact
official Rust1.98.1 source URLs above were retrieved. Neither the current docs nor
Cargo's MSRV is treated as the actual compiler pin. No test result follows from
this source review.
