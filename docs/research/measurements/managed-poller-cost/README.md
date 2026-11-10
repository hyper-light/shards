# Existing Unix Poller baseline harness

This checkpoint implements a **ROOT-only, unexecuted native workload** using
`shards_vmm::platform::Poller` (locally aliased `UnixPoller`) and native
`std::os::unix::net::UnixStream::pair`. It changes no platform/production source
and chooses no replacement for the existing native batch of 64. It supplies a
method for measuring that baseline, not an optimization, reproduced production
bug, allocation profile or completed performance result.

POLICY_READ IMPLEMENTER: canonical main
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`, policy files `f5cec365`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Read FULL canonical AGENTS.md, development-rules.md R01–R16,
development-evidence.md, CLAUDE.md, managed-runtime-plan.md and
`/Users/adalundhe/g121-handoff.md` before work. Coordinator Kiln
`01a148884a4770fdaf0111f3f792a248`; implementer
`01a1c5d2ee5871a2a9a9cdc9f56793f0`. Owned paths are only this new directory.
Minimal proposal sent before harness code under stable key
`implementation-refactor-poller-minimal-proposal-v1`, with the public API/FD
seam clarification under `implementation-refactor-poller-api-clarification-v1`.
No children or main merge. This checkpoint does not release the persistent lane.

The existing clean worktree was reused on `p/managed-poller-cost` from
`e1cb83139551b404736bc036277ccdb536ad409d`. Preserved branches/checkpoints:
`e1637959af1dbae631aa6f91c0d4d227f4c7dfac` shared assertions;
**unmerged** `82425ae5d20cb80d4385ba571936cfa537caa25e` ABI tests; and e1cb's
conditional terminal proposal. The unchanged ABI build.rs includes test source
in IDENTITY, so 82425 still requires host and guest rebuilding together.
This harness does not incorporate that ABI change or execute a guest.

## Method and limits

Read the constants lane's
[`managed-poller-capacity.md`](../../managed-poller-capacity.md) before choosing
any candidate. At this checkpoint that document remains in the separately
owned `managed-poller-research` worktree/branch; it is not copied or edited here.
Its pinned Linux/XNU/libevent/libev/mio alternatives remain unselected. The
concrete baseline is `crates/vmm/src/platform/unix.rs::Poller::wait`, which clears
the existing caller Vec, allocates native event storage on the stack and requests
the existing batch. **Caller Vec capacity is retained across clear.** Capacity
growth alone does not count allocation calls, bytes, copies or faults.
[Rust Vec contract](https://doc.rust-lang.org/std/vec/struct.Vec.html#method.clear),
[native stream-pair contract](https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html#method.pair),
[epoll wait contract](https://man7.org/linux/man-pages/man2/epoll_wait.2.html),
[archived Apple kqueue contract](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kqueue.2.html).
The finite fixture is an alternative to editing/instrumenting the production
poller. Its smaller scope does not substitute for an exact production caller's
reproduction or its allocation/cancellation measurements.

All four positional inputs to [fixture.rs](fixture.rs) are required:

```text
managed-poller-cost READY_COUNTS ROUNDS DEADLINE_MS MAX_WAIT_RECORDS
```

`READY_COUNTS` is a comma-separated list of positive source counts, in caller
order; repeated/decreasing counts are retained. `ROUNDS` is positive rounds per
count. `DEADLINE_MS` establishes **one absolute monotonic deadline for the whole
invocation**, covering setup, rounds and time between cases; it is never reset
after an empty/EINTR wait. Cleanup attempts occur unconditionally even when that
deadline has expired. `MAX_WAIT_RECORDS` bounds the total recorded waits across
all cases, including empty successful waits. Too few records, overflow,
unavailable descriptors or observations, allocation refusal and native errors
produce an incomplete result, never a relaxed pass. The caller owns admission;
finite CLI inputs are not a hard process/kernel budget.

For each count, the fixture creates one real existing Poller and exactly that
many held stream pairs, makes both ends nonblocking and checks every registration.
Each round writes exactly one known fixture byte to every source. It then waits
with the remaining deadline, rejects foreign/duplicate tokens, reads that exact
byte and requires a second read to return WouldBlock while the writer remains
open. All expected tokens must be seen before another round starts. One caller
`Vec<u64>` is reused across waits, rounds and count cases, making cold/growing/
retained capacity visible. Token uniqueness is a property of these fixture
registrations, not a new Poller API restriction.

Each wait retains raw tokens, elapsed `wait_ns`, caller capacity before/after,
round/index and native error where supplied. Each successfully drained token
retains write-completion→wait-return and wait-return→drained elapsed samples.
Write completion is an application timestamp, **not a kernel readiness timestamp**.
The first metric includes subsequent source writes and earlier handler/recording
work; the second includes preceding handlers in that returned batch. Preparation,
record copying and safe reporting are not subtracted or called zero-cost. This
does not instrument the syscall's internal allocation, native array initialization
or kernel service time. Recording allocation refusal can leave a wait without a
complete sample; the case fails and must not enter a successful distribution.
The unchanged Poller itself still uses its existing infallible Vec extension.

After each success/error/partial setup, every successful registration is explicitly
removed and its result retained; all pairs and the Poller are dropped before
reporting. Existing `platform::open_descriptors()` samples baseline/acquired/
released FD counts. Acquired must equal baseline plus `2 * held_pairs + poller`,
with checked arithmetic; released must equal the known baseline. Unknowns fail.
These are sequential process-level counts in a single-threaded owned fixture,
not a general proof of descriptor identity, kernel-memory return or leak freedom.
An external timeout/kill can interrupt cleanup and is explicitly incomplete.

No process-exit watch, empty-interest wait, persistent-hot-source fairness,
registration race, control thread, namespace, VM, BPF, privileged change or new OS
adapter is introduced. This native baseline covers Linux amd64/arm64 GNU/musl
and macOS **arm64 only**; Windows' existing different Poller is outside scope.
Actual native execution is unrun on every platform. Supported seven-target and
actual hardware coverage are not inferred from a Unix fixture or pure test.

## ROOT invocation and retained artifacts

[run.py](run.py) builds **and executes descriptor/latency work**. Implementers
must not run it. ROOT chooses a clean exact source checkout, a **new private** work
directory, native target, finite inputs, resource admission and command timeout:

```text
python3 docs/research/measurements/managed-poller-cost/run.py REPO NEW_WORK \
  --revision EXACT_COMMIT --target NATIVE_TARGET \
  --ready-counts CALLER_COUNTS --rounds CALLER_ROUNDS \
  --deadline-ms CALLER_DEADLINE --max-records CALLER_RECORD_LIMIT \
  --command-timeout-seconds CALLER_COMMAND_TIMEOUT
```

The wrapper refuses a mismatching revision, tracked source changes, an existing
work directory or an out-of-scope target. It copies only the harness, links the
actual named checkout's VMM through a path dependency and uses a new target with
one Cargo build job. It records source/lock/harness/binary hashes, the raw native
batch source token, actual OS/architecture, `rustc -vV`, Cargo version, dirty/untracked
path report, exact commands/status and stdout/stderr hashes. It retains resolved
Cargo.lock, raw JSONL and nearest-rank n/p50/p90/p99/max summaries. These are
baseline distributions only; there is no competitor or ratio. `--offline`
refuses missing dependencies rather than changing the input silently.

ROOT should review/compile the whole harness before execution and choose counts
on both sides of the **existing** batch, plus repeated and decreasing demand,
within its admitted envelope. No ready count, deadline, record cap or observed
winner becomes a production number. All actual descriptor/latency runs, guard
mutations, tracing, allocations/copies/faults, CPU, parent/kernel/neighbor costs,
paired workloads and credible competing baselines remain ROOT-owned and unrun.
Resource-lane allocator/CPU/fault instrumentation is deliberately not duplicated;
coordination message key: `implementation-poller-resource-scope-v1`.

## Actual pure/light evidence

[accounting.rs](accounting.rs) is dependency-free and opens no descriptors.
[light-check.py](light-check.py) copies only that actual source into a new private
source/target, compiles its test binary and runs one test thread. It never builds
or executes fixture.rs, VMM, a guest, socket or a profile.

Actual Kiln `machines.run`: Fedora runner
`8fadc1ec213332e16cb0d915a3fb913f`, Linux 6.19.10-300.fc44, glibc 2.43,
amd64, rustc 1.99.1 (58cf8f9c35b355199727627d07ae34a4e2846d67), LLVM 23.1.1.
Admission: **1 CPU / 1 GiB / 2 GiB**, `sync:false`, private
`/home/adalundhe/managed-poller-cost-light-e1cb-v1`; headroom reported 27 idle
cores/48 GB free. ROOT clones/targets were untouched. Source-only local work
continued when ROOT reported this Mac below its 30 GiB floor; no local build,
compile, target, full worktree, mirror or large archive was created.

```text
rustfmt --edition 2024 --config skip_children=true accounting.rs fixture.rs
exit 0; exact formatted sources returned to this owned directory
python3 scripts/check-development-policy
exit 0; evidence/coordinator review is still required
python3 light-check.py SOURCE NEW_PRIVATE --target x86_64-unknown-linux-gnu
rustc --test --edition 2024 --target x86_64-unknown-linux-gnu accounting.rs -o NEW_PRIVATE/target/accounting-tests
accounting-tests --test-threads=1
5 passed; 0 failed; 0 ignored; exit 0
```

The five tests cover explicit retained input order; missing/malformed/zero and
insufficient record inputs; descriptor/round arithmetic overflow; duplicate/
foreign/missing tokens and valid continuation/reset; and exact known FD arithmetic
without accepting unknown observations. They assert synthetic counts only, not
actual readiness, drains or descriptor return. Formatted/tested accounting SHA256:
`f4967f978c06cc19e7ace30b49ff593f8f162c666851e956cb79a93b1983afa7`.
Formatted-only fixture SHA256:
`f63ac590a61b70ad656d5e9f94983237a88d9e1e7d655e6625242dadf7f2a39f`.
[light-result.json](light-result.json) retains exact commands, original logs,
toolchain and hashes. **Complete harness compilation is unrun**, as are scoped
full-harness Clippy, all socket/latency execution and all non-GNU checks.

[light-supplemental.json](light-supplemental.json) retains a second admitted
source-only check: Python AST syntax, three pure nearest-rank cases and exact
Rust format checks passed. The initial `git diff --no-index --check /dev/null`
invocations returned 1 for added-file differences, with no whitespace diagnostics;
those actual nonzero results are retained, not called passing exit codes. A new
tiny private source directory with its own empty Git metadata and intent-to-add
entries then ran ordinary `git diff --check`, exit 0. No checkout was cloned and
no native fixture executed. Both checks used 1 CPU/1 GiB/2 GiB Kiln admission.

## R01–R16 and remaining gates

R01/R04/R06/R07/R13: production unchanged; finite explicit recording/drain/error/
cleanup steps and typed local rejections; fallible harness provisioning before
waits where possible. Existing Poller/library allocation behavior is not hardened
or proven panic-free. ROOT owes whole-harness compilation/review and actual
failure/cleanup witnesses. R02/R03/R05/R12: existing platform seam, pinned constants
research and primary contracts; isolated baseline versus production instrumentation
is the bounded choice. No new production literal, wait/cache/manager/thread or OS
adapter. All workload sizes/deadlines/records are explicit caller inputs; the
single-byte payload follows the exact-byte fixture contract, two FDs follow a
native pair, one FD follows Poller ownership, and percentiles use the named
nearest-rank definition. R08/R14: **all performance/profile/competitive results
unrun**; no 4–5×/allocation-free/leak-free claim. R09/R10: no production bug claimed
or fixed; source method and actual pure evidence retained, decisive native/
mutation results owed to ROOT. R11: only GNU pure checks passed; remaining targets,
libcs and actual hosts unverified; Intel Mac unsupported and Windows ARM paused.
R15: implementer source/light checkpoint, all audits/campaigns/profiles/main merge
and combined revision gates ROOT-owned; no children. R16: reused worktree and
remote private tiny target; paused VM, protected caches, source and evidence
preserved. Current terminal source finding remains conditional until ROOT's exact
live reproduction/authorization; it is not fixed by this harness checkpoint.
