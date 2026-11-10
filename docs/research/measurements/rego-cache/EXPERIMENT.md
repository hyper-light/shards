# Regex compilation ownership experiment (not adopted)

The actual Program/Machine experiment removes the reproduced history-dependent
answers and dynamic retention in its selected test variants. It supports **lazy
compilation owned by an immutable policy** as a candidate. It does not authorize
production adoption: fallible allocation/admission, broader semantics and target
coverage remain open. The default production cache behavior is unchanged.

The clean signed proof remains `593fe5d7eb6e8f7465414213ef23c527bb37cf9f`, branch
`p/managed-rego-cache`. This separate branch, `p/managed-rego-cache-experiment`,
contains test-only candidates and measurement hooks. Do not merge the candidates
as a production cache decision. The earlier cancellation checkpoint is untouched.

## Identity and actual execution

`POLICY_READ`: canonical `c921a0b12b06d495b354907162e991376edffd41`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`, implementer
`/root/constants_research`; coordinator `/root`. All R01–R16, evidence checklist,
AGENTS, CLAUDE, managed-runtime plan and current handoff were reread. The coordinator
approved the additional experiment ownership of `eval.rs` Program/Machine,
`funcs/mod.rs` Context/Builtin, `funcs/regex.rs` and this measurement directory.
No subagent was spawned and no main merge was performed.

Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, Linux amd64 GNU
`6.19.10-300.fc44.x86_64`, Rust `1.98.1 (48a229cea 2026-09-01)`, Go `1.26.1`.
OPA v1.14.1 commit `3d1bac44a21906cc7f118ae535b78e39185214a5`, module checksum
`h1:MhurLB9mSbXmojYFCmGbiC1Uagu1+aFAV4XVotDA86M=`. CEL-Go v0.27.0 commit
`450089b38d4c77840bae9ef4b284bb4b96d41ac8`, module checksum
`h1:e7ih85+4qVrBuqQWTW4FKSqZYokVuc3HnhH5keboFTo=`.

The envelope stayed **4 CPUs / 8 GiB RAM / 10 GiB disk**, `-j4`, Go `-p=4`,
`GOMAXPROCS=4`. Work used only the private synced clone
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-rego-cache-experiment-7a783fdd6c`,
`~/managed-rego-cache-experiment-target`, Go caches
`~/managed-rego-cache-experiment-go-{build,mod}`, and separate private OPA/CEL
source copies. Inactive lane caches were reflink-copied without changing their
sources. Evidence lives in `~/managed-rego-cache-experiment-evidence`; frozen
executables remain there. Other implementers' clones/caches were not modified.
No Mac build/test ran; the final local free-space check reported 32 GiB.

Kiln r28/r30/r31 were exploratory campaigns. Their actual logs are preserved, but
r33 supplies the final freshness-controlled results and gates. Kiln synchronization
can preserve source mtimes; a byte restoration alone is insufficient. The final
scripts touch only owned Rust sources and clean only the private `shards-rego`
`test-release` package artifacts before compiling each variant/mutation/restoration.
Both final Rust compiler-artifact records say `fresh:false`. The scripts freeze
and hash the actual executed executable, retain compiler JSON/logs, source hashes,
load observations and CPU description. No dependency or other-agent cache is
rewritten to establish freshness.

Executed binary SHA-256:

| Binary | SHA-256 |
| --- | --- |
| Rust allocation counters enabled | `4bb97d05fcb779b59986cbe32a2e155c23c4cf4430562f5d792d94ae19dba660` |
| Rust counters compiled out | `c2c74ae3dc1752fdca97d2f4e7a58dc0c5f6c9034fe1efa8894b326939fa7512` |
| OPA oracle (both campaigns) | `f944e539c5c389441ac36c90346e2581376b24e042dae8e9f3c1887b18622f65` |

## Candidate and primary alternatives

[The initial live proof](README.md) establishes the mixed-kind/delimiter defect,
actual retention and exact upstream parity before these experiments. Primary
source revisions, byte counts and hashes are in [sources.json](sources.json).

- OPA's [builtin declarations](https://raw.githubusercontent.com/open-policy-agent/opa/v1.14.1/v1/ast/builtins.go)
  and [regex reference](https://www.openpolicyagent.org/docs/policy-reference/builtins/regex)
  define results using the supplied regex/template, input and delimiters. The
  [pinned implementation](https://raw.githubusercontent.com/open-policy-agent/opa/v1.14.1/v1/topdown/regex.go)
  shares a map keyed only by pattern text; its template insertion bypasses the
  ordinary cap. This source explains the live symptom; it does not justify a cap.
- OPA's [prepared Rego API](https://raw.githubusercontent.com/open-policy-agent/opa/v1.14.1/v1/rego/rego.go)
  is the live oracle seam here. No OPA production source is patched. The prior
  same-module diagnostic provides the otherwise private retention observations.
- [CEL-Go's planner optimization](https://raw.githubusercontent.com/google/cel-go/v0.27.0/interpreter/optimizations.go)
  binds a compiled constant regex into the planned call. Its
  [public option](https://raw.githubusercontent.com/google/cel-go/v0.27.0/cel/options.go)
  documents eager program-creation errors. Its
  [dynamic string matching](https://raw.githubusercontent.com/google/cel-go/v0.27.0/common/types/string.go)
  calls `regexp.MatchString` instead of building a pattern-history map. This is an
  established alternative, not interchangeable Rego semantics.
- [Rust regex documentation](https://docs.rs/regex/1.13.1/regex/#avoid-re-compiling-regexes-especially-in-a-loop)
  motivates measuring compilation reuse. It does not supply memory authority or
  a suitable number of retained dynamic patterns.

All three Shards modes use parser → real Compiler → Program → fresh Machine and
Context → `eval_query` → real builtin dispatch. The compiler is dropped before
execution. OPA parses the same module and evaluates the same JSON inputs through
`PrepareForEval`/`EvalInput`. Inputs are generated before timing, are entirely
synthetic and require no credentials/provider calls.

`current` selects the existing thread cache. `uncached` compiles each executed
call and retains no regex history. `program` selects an experiment-only pool
owned by Program; Machine/Context hold a Weak binding. This is not a public API
or a replacement evaluator. Non-test edits merely pass the existing Context to
the private helper; all candidate state and branches are behind `cfg(test)`.

The candidate enumerates literal compilation identities from the compiled module
AST using an iterative walk: `(ordinary, pattern)` or `(template, start byte,
end byte, pattern)`. It deduplicates identical constant identities within that
Program. This is policy-owned reuse, not a strict slot per source call. Entries
transition `Uncompiled → Ready(compiled)` or `Uncompiled → Failed(cause)` only when
an executed builtin requests them. Outer evaluation still adds the actual call's
location. Invalid operands/delimiters are validated at the existing builtin seam
before compilation. Dynamic patterns or delimiters miss and compile without
inserting. An identical dynamic value may reuse an already-owned literal identity;
it cannot enlarge the set. Program drop destroys that set after live calls end.

The metadata cardinality derives from AST identities rather than an invented cap.
**Cardinality is not memory entitlement.** Both the prototype's BTreeMap/Rc/Vec
allocation and the existing regex engine can fail through infallible allocation.
No admitted heap budget has been demonstrated at this seam. These are explicit
R01/R03 adoption blockers, not exceptions granted by `cfg(test)` measurements.

## Policy outcomes, errors and cleanup

Five policies each receive 2,000 inputs, with a fresh query/evaluation context per
input. Hot constants, distinct dynamic regexes, distinct dynamic templates and
conditional failures use equivalent policy text/input/output semantics in OPA and
Shards. Every timed successful result is checked; conditional failures report
1,000 builtin errors with the same first error text and undefined result. First
execution is recorded separately. The initially disabled invalid pattern remains
`Uncompiled`; the first executed failure changes only that entry to `Failed`.

The extra error-order policy contains two invalid patterns. The first query reports
only the first executed error and leaves the second entry uncompiled; an explicit
second query then reports the second error. The exact two rendered errors, names,
locations and order agree across all Shards modes and pinned OPA. This exercises
execution-time behavior that eager CEL optimization would change.

| Mixed policy sequence | Current Shards / OPA | Uncached / Program | Independent per-call meaning |
| --- | --- | --- | --- |
| ordinary → template, `a.b` / `axb` | `[true,true]` | `[true,false]` | `[true,false]` |
| template → ordinary, same | `[false,false]` | `[false,true]` | `[false,true]` |
| template angle → square, `a<b>` / `ab` | `[true,true]` | `[true,false]` | `[true,false]` |
| template square → angle, same | `[false,false]` | `[false,true]` | `[false,true]` |

With dynamic delimiters alternating angle/square, current Shards and OPA return
**1,000 incorrect answers** according to independent-call meaning; both candidates
return zero mismatches. Their different answers are a deliberate upstream-bug
deviation, excluded from equivalent speed ratios. Passing a measurement collector
for current/OPA does not mark those semantics correct.

After the 2,000 distinct dynamic templates, current Shards retains 2,000 entries;
ordinary dynamic regexes retain 100. Program owns zero slots/compiled values for
all dynamic policies; uncached owns zero legacy entries. Fresh queries therefore
do not accumulate dynamic compilation history in either candidate. Hot Program
owns exactly two ready identities; conditional failure owns one failed identity.
Two simultaneous threads create independent Programs, each holding one ready of
four declared identities. After each Program drops, its Weak pool reference no
longer upgrades. This establishes tested pool ownership, not arbitrary allocator
RSS return, a cross-thread shared Program API, or freedom from every leak.

In the profiled dynamic-template run, cumulative in-batch Rust requested-byte
retention increased 31,509,395 bytes for current and zero for Program. Counts
include execution/fixture allocations and do not isolate exact cache-only bytes.
OPA GC/heap deltas are recorded separately; dropping a prepared query does not
clear its global regex map. Source/policy drop observations are one sample each,
not distributions of cleanup latency.

## Measurements and limits

Each reported distribution contains **20 batches × 100 evaluations**; p99 is the
maximum of only 20 batch means, not an individual-request tail estimate. Setup,
first call and owner drop are separate single observations per policy/mode.
Full p50/p90/p99/max for wall, process CPU, allocations/reallocations, requested
bytes, retained delta, faults and context switches are in
[experiment-report.json](experiment-report.json); raw batch samples are archived.

Counter-disabled median wall microseconds per evaluation (r33):

| Equivalent workload | Current | Uncached | Program | OPA |
| --- | ---: | ---: | ---: | ---: |
| Hot constants | 10.423 | 57.620 | 10.552 | 15.726 |
| Dynamic regex | 17.768 | 17.243 | 9.584 | 15.699 |
| Dynamic template | 44.938 | 32.843 | 19.831 | 27.531 |
| Conditional failed compilation | 7.827 | 8.503 | 4.575 | 15.323 |

The allocation-profiled campaign has materially different timings: Program hot
6.760 µs, dynamic regex 9.530 µs, dynamic template 17.727 µs, failure 4.818 µs;
OPA respectively 11.469 / 12.505 / 15.409 / 7.507 µs. In particular the profiled
OPA template median beats Program. The campaigns ran sequentially on a shared
host, and process order was fixed, not interleaved/randomized. Counter-disabled
runs sometimes being slower is evidence of substantial variation, not proof that
counters speed up execution. These data support testing the seam; they do not
establish stable competitive ratios. No 4–5× overall target is achieved.

Allocation-profiled median per-query counts/bytes:

| Workload/mode | Rust allocations | Rust requested bytes | OPA mallocs / TotalAlloc bytes |
| --- | ---: | ---: | ---: |
| Hot current or Program | 161 | 19,725 | 119 / 6,856 |
| Hot uncached | 635 | 102,720 | 119 / 6,856 |
| Dynamic regex Program | 223 | 27,045 | 132.96 / 9,372 |
| Dynamic template Program | 306 | 54,510 | 179.56 / 12,760.8 |

Program preserves hot reuse's measured allocation benefit over uncached; dynamic
history does not provide a benefit in these churn inputs. Rust remains more
allocation-heavy than OPA under these definitions. Rust counters wrap System,
forwarding all four allocator operations unchanged, and count requested layouts;
Go uses [runtime MemStats](https://raw.githubusercontent.com/golang/go/go1.26.1/src/runtime/mstats.go)
with its own runtime/GC accounting. These byte definitions are not identical.
The second Rust executable compiles counter updates out and marks
`allocation_profiled:false`; zeros there are unmeasured, not allocation-free.

`getrusage(RUSAGE_SELF)` records process CPU/faults/switches per batch; `/usr/bin/time`
records whole-process RSS/faults. These include the evaluator and test controller.
Copies, actual kernel bytes, PSS, contention, parent orchestration and entitled
neighbor impact have not been isolated. No resource-budget or fairness claim is
made from these measurements.

## Live CEL alternative

Kiln r46 calls CEL-Go's actual `Env.Compile`, `Env.Program`, `OptimizeRegex` and
`Program.Eval` APIs, with a wrapper around its published regex factory to count
planner compilations. Four 2,000-input cases pass. The planned constant calls the
factory once; dynamic churn calls it zero times. The AST variable is dropped
before execution. Process heap observations before/held/after drop are retained,
but GC/runtime differences do not prove exact compiled-object destruction.

Within CEL only, baseline vs planned hot medians are 3.153 vs 0.299 µs and 37 vs
3 allocations (2,728 vs 64 TotalAlloc bytes). Dynamic medians are 2.498 vs 2.556 µs,
both 22.09 mallocs and 2,589.76 bytes. This is a different language, single ordinary
regex expression and runtime path; it is **not** an equivalent OPA/Shards speed
comparison. CEL has no tested Rego template operation here.

The live eager-error probe confirms the compatibility limit: with `enabled=false`,
unoptimized `enabled ? value.matches("[") : false` evaluates to false; selecting
CEL's regex optimization rejects Program construction for the unexecuted pattern.
The Shards candidate therefore retains CEL's ownership idea with lazy compilation,
not its eager error timing. CEL original source bytes were compared with the
checked module source. The probe passed; the trailing module-inventory command
failed because Go selected vendor mode. Correcting that command to `-mod=readonly`
then passed. This orchestration failure is retained and is not a semantic red.

## Independent mutations and final gates

Each mutation starts from baseline, changes one condition, forces a private package
rebuild and records the executed binary/source hash. The failures are actual policy
results, retained objects or ownership effects, rather than a missing alert:

| Mutation | Observed failure, exit 101 |
| --- | --- |
| Omit compilation kind from lookup | Mixed ordinary/template boolean changes; `constant compilation identity changed policy result` |
| Omit delimiter pair from lookup | Mixed angle/square boolean changes; same result assertion |
| Insert every dynamic miss | 2,000 retained dynamic identities; `dynamic expression retained compilation history` |
| Keep an additional strong pool owner | Weak pool still upgrades after Program drop; `program compilation owner outlived Program drop` |

Restoration and forced rebuild pass the dynamic-template plus mixed/error/owner
checks. Final r33 library run: **19 passed, 0 failed, 2 ignored**; both ignored tests
are explicit diagnostic campaigns, not silently passing gates. The experiment was
explicitly invoked for every final policy/mode; the prior proof diagnostic was run
in its signed proof batch. Strict all-target native `shards-rego` clippy passes.
Policy checker, owned-file rustfmt, whitespace, shell/Python syntax and Go formatting
pass. No all-target seven-platform or complete workspace pass is claimed here.

## Proposed D98 compatibility record and next implementation

**Draft only, coordinator review required:** regex/template answers depend on the
current operation's explicit arguments, irrespective of earlier regex operations.
Compilation identity includes kind and template delimiters. Shards intentionally
differs from OPA v1.14.1's shared-map history defect in the four named mixed-call
cases and alternating dynamic delimiters above. Existing regex syntax, literal
escaping, template anchoring, operand validation, undefined/error behavior and
execution-time error order remain required. Regenerate/extend the deliberate
D98 deviation/oracle fixtures before adopting that behavior. Do not label the
changed answers a semantics-free optimization or preserve known wrong answers
merely because pinned OPA also returns them.

Candidate implementation, subject to review: attach lazy compiled constant
identities to the actual immutable compiled Program/IR lifetime, using existing
compiler/builtin dispatch seams; compile dynamic values without retaining history.
Prefer reusing existing compiler traversal/call metadata over adopting this
experiment's independent AST walk. Derive metadata from the policy and reserve
its actual memory before creation; compilation must obey an admitted charge and
propagate a typed refusal. No replacement fixed cap, query-local map, parallel
manager or learned expansion of authority is justified. Measurements choose a
method, not a runtime number.

Before adoption: establish fallible container/regex construction and budget ownership;
exercise OOM/refusal and compile complexity bounds; handle public Program tree
mutation or make compiled ownership immutable; test all regex builtins, overrides,
partial evaluation and query-created constant expressions against the full oracle
corpus under candidate selection; cover policy reload/concurrent ownership and
remaining supported targets. Repeat performance in interleaved independent process
runs with actual contention/neighbor and copy/kernel/PSS attribution. This checkpoint
is the measured proposal, not an authorization to skip those gates.

## R01–R16 record

| Rule | Evidence and remaining gap |
| --- | --- |
| R01 | Candidate state/assertions/allocator counters are test-only. Native clippy passes. Infallible prototype containers, compiler/library allocations and OOM containment block production adoption. |
| R02 | Primary pinned OPA/CEL/regex/runtime sources, actual identical Rego inputs/outputs, live CEL alternative, method and contrary outcomes above. |
| R03 | No product cap/literal replacement. AST identity set is derived; entitlement and compile-byte bounds remain unresolved. Experimental 20/100/2,000 are sample sizes only. |
| R04 | Explicit kind/delimiter identity, Uncompiled/Ready/Failed and weak owner transitions; tested lazy errors/order/drop. Prototype borrow/expired-owner string errors need typed production treatment. |
| R05 | Real Program/Machine/builtin seam; no new public abstraction/default. Dynamic insertion independently fails and constant reuse is measured. Adoption/refactor still held. |
| R06 | Focused private test module and existing dispatch hooks. Iterative AST scan is prototype overhead, not a proposed parallel compiler framework; reuse actual compiler metadata before production. |
| R07 | Comments identify test scope, allocator safety, derived identity and no-history requirement. Primary rationale is retained here. |
| R08 | Pinned equivalent Rego policies, distributions/ratios and semantic-deviation exclusion. Shared-host variation and misses explicit; no achieved overall 4–5× claim. |
| R09 | Signed proof reproduces exact cache defects before experiment; all4 mutation failures observe the intended result/retention/ownership effect. |
| R10 | Repeatable harnesses, source/binary SHA-256, compiler JSON/logs, mutation patches, raw batches and all exploratory outcomes preserved. |
| R11 | Actual native Linux amd64 GNU only. Other six targets/libcs/distributions and hardware VM matrix unverified. macOS arm64 only; paused Windows ARM VM untouched. |
| R12 | Shared Rego seams, no installed binary/privilege/crypto change. OPA/CEL executables are temporary measurement artifacts only. |
| R13 | Three isolated alternatives before adoption; no production framework/manager. The complete test harness is retained for review, not product scope. |
| R14 | Rust allocator counters plus counter-disabled timing, Go MemStats, process CPU/fault/switch and owner cardinality/drop. Copies/kernel/PSS/neighbor and stable interleaved distributions remain owed. |
| R15 | Coordinator-assigned recurring lane, exact ownership/envelope, signed checkpoint with named paths; no children/main merges. Coordinator owns adoption and final combined checks. |
| R16 | Private resources, explicit freshness, unchanged admission request and preserved artifacts. Only own regenerable package artifacts cleaned. No headroom bypass or Mac build below floor. |

## Replay and artifacts

From an admitted Linux checkout with the pinned toolchains and private directories:

```sh
export CARGO_TARGET_DIR="$HOME/managed-rego-cache-experiment-target"
export GOCACHE="$HOME/managed-rego-cache-experiment-go-build"
export GOMODCACHE="$HOME/managed-rego-cache-experiment-go-mod"
export SHARDS_REGO_OPA_DIR="$HOME/managed-rego-cache-experiment-opa-v1.14.1"
export SHARDS_REGO_CEL_DIR="$HOME/managed-rego-cache-experiment-cel-v0.27.0"
python3 docs/research/measurements/rego-cache/make-workloads.py "$HOME/rego-inputs.json"
export SHARDS_REGO_EXPERIMENT_INPUT="$HOME/rego-inputs.json"
bash docs/research/measurements/rego-cache/mutate-experiment.sh "$HOME/rego-mutations"
bash docs/research/measurements/rego-cache/run-experiment.sh "$HOME/rego-profiled"
SHARDS_REGO_UNPROFILED=1 bash docs/research/measurements/rego-cache/run-experiment.sh "$HOME/rego-unprofiled"
python3 docs/research/measurements/rego-cache/summarize-experiment.py "$HOME/rego-profiled"
python3 docs/research/measurements/rego-cache/summarize-experiment.py "$HOME/rego-unprofiled"
bash docs/research/measurements/rego-cache/run-cel.sh "$HOME/rego-cel"
```

The OPA directory is a writable private copy of the pinned checked module as set up
by the proof's `run.sh`; it must not point at a module cache or another lane's source.
[experiment-fedora-r33.tar.gz](experiment-fedora-r33.tar.gz) contains 364 hashed raw
logs/statuses/compiler records/summaries and mutation patches, including exploratory
campaigns. [experiment-cel-r46.tar.gz](experiment-cel-r46.tar.gz) contains the live
CEL probe and inventory recovery. [experiment-manifest.json](experiment-manifest.json)
verifies each archived file. [experiment-source.patch](experiment-source.patch)
retains the exact source experiment relative to signed proof593. Final source HEAD
was proof593 plus this uncommitted experiment; the final source hashes identify
exactly the candidate now committed. Source and binary evidence were verified
locally after transfer. All lane jobs finished; private source/caches/binaries and
raw evidence remain preserved. Production adoption and combined gates remain with
the coordinator.
