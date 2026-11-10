# Unadopted C-stream-lock reporting experiment

TEST ONLY, `publish = false`, standalone workspace. No product source, dependency,
reporting API or platform policy changes. Based on `8e76824a400200b50cc514ebbb74bea78a89bc77`.
Signed source checkpoint: `3c223e7595c6faae09dfa82d79ba5f685a916eda`.

POLICY_READ implementer: full canonical AGENTS/R01–R16/evidence/CLAUDE/managed
plan/current handoff at `f5cec365d2e435afae5e6f613c92d9d3759ef60f`; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root owns all research selection, audits, allocation/fault/mutation campaigns,
profiling, benchmarks, stress, real E2E/VM, matrix and adoption decisions. This
implementer may run only the authorized ordinary native Mac arm64 light checks.

## Question, mechanism and lifetime

Root r338 copied exact `vm_run::report` with static Display: ordinary0, cold
`fail_after(0)` SIGABRT/-6/64B, warmed diagnostic0. Warming is never a remedy.
Root retains `~/root-managed-cold-stderr-probe-v1` on Macb24. This prototype is
an experiment prompted by that actual observation, not a product fix or a claim
that native C/runtime allocation has been characterized.

`report(message: impl Display)` formats the existing `shards: {message}\n`
representation directly into native fd2 writes while pairing one whole-operation
`flockfile`/`funlockfile` on the process's stderr FILE. There is no owned message,
buffer, cache, warm call, truncation policy or fallback output. The scalar-size
write bound is the ABI's `ssize_t::MAX`; fd2 is `STDERR_FILENO`, not a guessed
descriptor. Partial writes advance checked slices; EINTR repeats the interrupted
write, not a state probe. Other errors return, including EBADF on closed fd2 and
WriteZero. The first actual I/O error is retained even if Display ignores a
formatter error. Formatter failure remains a separate typed `fmt::Error`.

The function is unsafe because the entire call, its Display implementations and
all other threads must keep the exact stderr FILE live: no `fclose`, `freopen` or
pointer replacement. It does not create an owning/borrowed File, duplicate a
descriptor or close one. Tests close only fd2 inside their own private subprocess.
`StreamUnavailable` covers a null pointer before any native call; it does not
detect dangling or concurrently replaced FILEs. The unlock guard pairs native
calls on all ordinary return paths; this is not a measured lock-failure proof.

**This C FILE lock does not participate in Rust's private stderr lock.** It is
FILE-level locking, not descriptor-level locking. Mixed Rust/C writes, other raw
fd2 writers, arbitrary close/reopen, deadlocks/ordering, blocked sinks, native lock
failure, cancellation and actual resource cost remain OPEN. Nothing here
establishes compatibility, panic freedom, allocation freedom or product readiness.

## Coordinator-selected primary sources and alternatives

Pinned Apple Libc comparison commit:
`71bbe350ab79eef58113991d817ccc6165061a64`.
[primary-sources.json](primary-sources.json) retains the coordinator's exact URLs
and SHA256 records; source selection and gathering belong to root.

- `stdio/FreeBSD/findfp.c:79,92,99–107`: the three standard streams' extension
  objects use static recursive mutex initializers; `__stderrp` points to standard
  stream2. This source association is not proof it matches a runner's host build.
- `stdio/FreeBSD/_flock_stub.c`: public lock/unlock call native pthread operations
  **and discard their return status**, preserving errno. The public void APIs
  cannot make native lock failure observable or typed in this prototype.
- `stdio/FreeBSD/flockfile.3`: FILE ownership, blocking acquisition, recursive
  counts and paired unlock contract; POSIX.1-2001 comparison.
- Local Apple macOS26.4 SDK `_stdio.h:169,391,393`: `FILE *__stderrp` and the two
  exact `void(FILE *)` function declarations. Existing libc0.2.189 does not expose
  these Darwin symbols, so root approved only these test-local extern declarations
  using its existing `libc::FILE`. The lockfile pin/checksum match the main workspace.

Alternatives retained, unselected: standard Rust stderr retains its implementation
and Unicode semantics but has root's demonstrated cold allocation; a private
native recursive mutex adds another synchronization owner and needs initialization/
failure/lifetime research; a cloned raw File adds descriptor acquisition and lacks
whole-report serialization; a native Windows writer would add console conversion
work unjustified by this prototype. No alternative is adopted by these tests.

## Authorized light checks

The five controlled subprocess cases compare complete supplied expected bytes:
static prefix/newline; Unicode plus numeric formatting; recursive Display on the
same native thread; actual EBADF after closing only that child's fd2; formatter
failure with the already-written prefix and no fallback. Their stdout markers are
fixture result observations, not replacement report text. Parent capture buffers
are ordinary test-only output collection, not buffers in the reporting path.
No fault injection, native allocation measurements, concurrent/mixed-writer
campaign, mutation, benchmark, stress, VM or product integration is authorized
for this lane. Partial/EINTR logic exists but no interrupted/short-write proof is
claimed from these ordinary cases.

Native Macd132 job r80 completed0: **5 passed,0 failed**, format0, strict
all-target standalone clippy0 and development-policy0. Host macOS26.4 build25E246,
Darwin25.4.0 arm64; Rust1.98.1 (`48a229cea`), LLVM22.1.8; installed SDK26.5.
The SDK declarations are retained in the light metadata, separately from the
local26.4 declaration comparison and pinned Apple source association above.
The admitted2CPU/1GiB/2GiB envelope was unchanged; actual preflight free disk was
3,011,969,093,632 bytes and the new private compiler target was20,464KiB logical.
This is ordinary light validation, not allocation/cost characterization.

[light-evidence/raw](light-evidence/raw/) retains19 raw records: preflight/identity,
actual SDK declarations, commands, logs/exits, checked source hashes and frozen
binary metadata. The run compiled an uncommitted8e snapshot: the five source
hashes in `checked-source-sha256.json` and `after-source-sha256.json` equal the
signed3c223 source bytes. The earlier preflight `source-sha256.json` predates the
final FFI safety comment and is not the checked-source identity. No inherited
committed-revision pass is claimed. The tests' actual subprocess bytes were
compared against complete explicit expectations; individual case payload files
were not retained separately from those assertions/source and test logs.

Private clone:
`/Users/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-reporting-prototype-09e1efdc11`.
Private target: `/Users/adalundhe/managed-reporting-prototype-target`.
Both are inactive after r80; no own jobs remain and no further source sync or
compilation is planned. Frozen native fixture binary outside the target:
`/Users/adalundhe/managed-reporting-prototype-evidence/frozen-binaries/managed-reporting-prototype`,
552,080 bytes, SHA256
`60445ab3cc418239780579fa8562d49e4beea0e6d56bb1b09db0809aef2c5c39`.
No prior root/agent cache, local compiler, paused VM or other worker was used.

Caller invocation, on a private admitted Mac arm64 runner with existing tools:

```sh
: "${CARGO_TARGET_DIR:?new private prototype compiler target}"
lockf -k /tmp/g121-mac.lock cargo test --locked -j4 \
  --manifest-path docs/research/measurements/managed-reporting-coordinator/prototype/Cargo.toml \
  -- --test-threads=1 --nocapture
```

The assigned light envelope is2CPU/1GiB/2GiB. Keep the actual Mac free-space floor
at30GiB. No local compiler, old target/cache access, nested agents, main merge or
paused Windows ARM VM use. Other six target/OS builds are deliberately unrun:
the standalone package rejects unsupported hosts rather than report passing skips.
This experiment changes none of the product's seven-target coverage obligations.

## Owed coordinator proof

Root must retain separate exact-caller cold Rust/native allocation-refusal proof,
lock/FILE lifetime and close/reopen outcomes, mixed standard/native writer ordering,
blocking/cancellation, independent mutations/restoration, induced parent/kernel
resources and cost before selecting any product mechanism. No warm call, fallback,
static substitute, larger buffer or skipped red closes those obligations. Public
flockfile/funlockfile cannot expose native pthread return status; no typed native
lock-failure claim can be derived from the prototype's write/formatter errors.
Root may build/import the pinned source in a new private proof harness. A normal
invocation of this standalone package is not the actual product report/caller,
and its five light cases do not close r338 or the full R01 requirement.

## R01–R16 record

| Rule | Scoped proof / outstanding work |
| --- | --- |
| R01 | Checked write progress/typed outcomes and safety conditions; native5/0 and strict clippy0. Cold Rust/native allocation and actual caller safety are root-owned OPEN. |
| R02 | Root-selected pinned Apple sources/contract and unselected alternatives; no product selection or independent audit. |
| R03 | Native ABI count and standard fd identity only; controlled example texts/values are test inputs, not runtime policy. |
| R04 | Distinct unavailable-stream, I/O and formatter outcomes; first I/O failure retained. Native void lock failure remains unobservable. |
| R05 | One experiment, borrowed streaming, one per-call authoritative failure; no cache/polling/framework or product wait change. |
| R06 | One formatter sink, checked write loop and paired unlock guard; no process manager. |
| R07 | Safety comments identify live FILE and buffer-lifetime obligations; primary rationale above. |
| R08 | No benchmark or4–5x claim; all comparisons remain root-owned. |
| R09 | Root r338 actual cold standard-stderr failure precedes experiment; no reproduced native-candidate defect or fixed-product claim. |
| R10 | Signed3c223 source,19 raw records, before/after checked hashes and frozen binary retained; all root refusal/mutation/resource proof explicitly owed. |
| R11 | Native Mac arm64 experiment only; no other platform, seven-target or VM pass inherited. |
| R12 | No product adapter/API or installed binary; only exact test-local Darwin declarations, existing libc pin. |
| R13 | Standalone unpublished source under one owned experiment path; no generic reporter or dependency framework. |
| R14 | Allocation/fault/kernel/parent/cost behavior unmeasured and root-owned. |
| R15 | Scoped implementation/light checks, signed named-path checkpoint/required trailer; no children agents/main merge or root cache. |
| R16 | Private admitted runner/cache only, preserve earlier artifacts and Mac floor; no cleanup or local compiler. |
