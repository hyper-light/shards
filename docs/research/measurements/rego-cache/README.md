# Regex/template cache: live pinned OPA and Shards proof

Status: **fixture/research checkpoint only**. No production cache code or behavior
changed. Both the sequence-dependent answers and template retention beyond the
stated maximum reproduced live. Correcting them requires an explicit D98
compatibility record and measured ownership/compilation experiment; those are not
completed by this proof.

## Identity, scope and reproduction

`POLICY_READ`: canonical revision `c921a0b12b06d495b354907162e991376edffd41`,
`docs/development-rules.md` SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`, role
`implementer`, lane `/root/constants_research`, coordinator `/root`. AGENTS, all
R01–R16, evidence checklist, managed-runtime plan and full handoff reread. Owned
paths: `crates/rego` focused fixture and this directory only. The earlier clean
cancellation checkpoint `c47de57f` is unchanged.

Branch `p/managed-rego-cache`, private synced checkout
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-rego-cache-f6841de0de`,
Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, Linux amd64 GNU
`6.19.10-300.fc44.x86_64`. Rust `1.98.1`; Go `1.26.1` (the Go 1.26 family used by
this repository's buildx oracle). OPA v1.14.1 resolves to commit
`3d1bac44a21906cc7f118ae535b78e39185214a5`, module sum
`h1:MhurLB9mSbXmojYFCmGbiC1Uagu1+aFAV4XVotDA86M=`.

The assigned envelope remained **4 CPUs / 8 GiB RAM / 10 GiB disk**. Initial Linux
capacity was below it; execution waited until 23 idle cores were available.
`-j 4`, Go `-p=4`, `GOMAXPROCS=4`; private Rust target
`~/managed-rego-cache-target`, private Go caches `~/managed-rego-cache-go-{build,mod}`,
and private OPA source `~/managed-rego-cache-opa-v1.14.1`. The Rust target was
reflink-copied from this lane's inactive cancellation target without modifying it.
No other agent's checkout/cache was used or altered. No local compiler/test ran.

Run from the admitted Linux checkout:

```sh
export CARGO_TARGET_DIR="$HOME/managed-rego-cache-target"
export GOCACHE="$HOME/managed-rego-cache-go-build"
export GOMODCACHE="$HOME/managed-rego-cache-go-mod"
export SHARDS_REGO_OPA_DIR="$HOME/managed-rego-cache-opa-v1.14.1"
bash docs/research/measurements/rego-cache/run.sh "$HOME/managed-rego-cache-evidence/reproduced"
```

`run.sh` downloads OPA through Go's normal checked module mechanism into the private
module cache, copies it to a separate source folder and adds only the diagnostic
`cache_probe_test.go`. It compares original `regex.go` and `regex_template.go` bytes
before and after. It calls the real `topdown.GetBuiltin` entry point, as the existing
`scripts/rego/oracle_test.go` does. The private test is in package topdown so it can
observe cache cardinality and clear state between experiments without exporting an
API, editing OPA production code, or using unsafe symbol access.

The Shards diagnostic is inside the existing `#[cfg(test)]` module. Production bytes
before that module remain exactly canonical. It calls real builtin lookup/functions,
creates and drops a fresh Context for each call, and reads the real private cache.
The explicitly invoked measurement is ignored in ordinary unit runs because its
strict mode intentionally fails on the demonstrated existing defect. It is not a
passing product guard or an excuse to skip one. All inputs are synthetic local
strings; no host files, credentials or provider operations are involved.

## Reproduced mixed-call outcomes

Kiln r24 completed exit 0 after checking two ordinary probes pass, OPA/Shards
outputs agree, and both independent strict guards fail as expected. Cases and
arguments are in [cases.json](cases.json). Each distinct call also appears first
with a fresh empty cache in a control scenario; `compare.py` derives the expected
independent-call answers from those actual cold observations and checks the case
expectations against them.

| Sequence (same pattern text) | OPA v1.14.1 | Shards | Actual independent cold-call controls |
| --- | --- | --- | --- |
| regex.match → regex.template_match, `a.b` against `axb` | `[true,true]` | `[true,true]` | `[true,false]` |
| template → ordinary, same arguments | `[false,false]` | `[false,false]` | `[false,true]` |
| template `< >` → `[ ]`, `a<b>` against `ab` | `[true,true]` | `[true,true]` | `[true,false]` |
| template `[ ]` → `< >`, same arguments | `[false,false]` | `[false,false]` | `[false,true]` |

Each call leaves one entry after its Context drops. Both a compilation-kind change
and a delimiter change reuse that entry. The strict independent-call guard fails:
OPA **exit 1**, Shards **0 passed; 1 failed, exit 101**. The normal observation
probes pass (Shards **1 passed; 0 failed**, OPA test PASS). A normal probe passing
means the experiment collected results; it does not mean the product is correct.

The first r21 build failed only in the new Rust fixture because `Value::string`
accepts `Into<Rc<str>>`, not `&String`. The fixture now passes `&str`. Its failed
compiler diagnostics remain preserved; that failure is not a product reproduction.
No production fix or cache-guard removal has been performed, so there is no claimed
post-fix green or mutation result in this checkpoint.

## Actual retention

Both implementations retain the same entry counts after every evaluation Context
has dropped:

| Distinct calls | Ordinary regex entries | Template entries | Template entries after one extra ordinary call |
| ---: | ---: | ---: | ---: |
| 100 | 100 | 100 | 100 |
| 200 | 100 | 200 | 200 |
| 400 | 100 | 400 | 400 |

This demonstrates that the stated 100-entry maximum does not bound the shared map
when templates are used. The source has no template-eviction check. It is not an
exhaustion experiment or proof that an unlimited amount of physical memory exists.
Shards additionally held the same 100 patterns simultaneously on two threads and
observed **100 entries in each, 200 total**: its cache is thread-local, whereas the
pinned OPA fallback map is process-global.

Shards used Weak references to actual compiled objects: after the Context dropped,
the cache still owned them; after explicit diagnostic cache clearing, those objects
were gone. This establishes compiled-object ownership for the tested cases, not
allocator/RSS return or resource safety for arbitrary hostile policies. Hash-map
capacity and allocator arenas can remain after clearing. Query drop does not clear
the product cache.

OPA GC observations for template batches (single observations, not distributions):
retained process HeapAlloc increased by 263,984 / 575,440 / 1,204,608 bytes after
100 / 200 / 400 calls; allocations during those batches totalled 643,144 / 1,309,304 /
2,657,080 bytes and 8,178 / 16,675 / 33,823 mallocs. These runtime totals include
fixture/runtime costs and are not isolated cache-byte attribution. Rust allocator
counts, copies, retained heap bytes, kernel charge and workload timing distributions
are not measured in this initial proof. Whole-process time/RSS/fault records are
retained as descriptive observations, not a comparative performance claim.

## Primary semantics, upstream behavior and alternatives

Exact fetched bytes/revisions/hashes are in [sources.json](sources.json).

- [OPA v1.14.1 builtin declarations](https://raw.githubusercontent.com/open-policy-agent/opa/v1.14.1/v1/ast/builtins.go),
  `RegexMatch`/`RegexTemplateMatch`, describe separate regex/template operations and
  explicitly name the template delimiter operands. The
  [official regex reference](https://www.openpolicyagent.org/docs/policy-reference/builtins/regex)
  describes matching against the supplied template and delimiter arguments. It
  does not specify dependence on unrelated previous calls.
- [Pinned regex.go](https://raw.githubusercontent.com/open-policy-agent/opa/v1.14.1/v1/topdown/regex.go),
  `getRegexp` and `getRegexpTemplate`, share `regexpCache[pat]`; the latter omits
  compilation kind/delimiters and its insert omits eviction. `getRegexp` uses the
  optional caller-provided inter-query cache when configured, but the template
  path always uses the fallback global map. The normal fallback cap is an upstream
  default, not a justified Shards resource entitlement.
- [Pinned template compiler](https://raw.githubusercontent.com/open-policy-agent/opa/v1.14.1/v1/topdown/regex_template.go)
  quotes literal fragments, compiles delimited fragments and anchors the result.
  An ordinary regex compiler interprets `.` as a wildcard. This explains the live
  cold controls and why the compiled object must include the compilation mode and
  delimiters in its identity.
- [regex 1.13.1 documentation](https://docs.rs/regex/1.13.1/regex/#avoid-re-compiling-regexes-especially-in-a-loop)
  documents compilation cost and reuse of engine allocations. That supports
  measuring reuse, not retaining arbitrary historical dynamic patterns forever.
- [CEL-Go v0.27.0 optimization](https://raw.githubusercontent.com/google/cel-go/v0.27.0/interpreter/optimizations.go)
  compiles a constant regex into the owning planned call, whose closure holds it;
  [its planner interface](https://raw.githubusercontent.com/google/cel-go/v0.27.0/interpreter/interpreter.go)
  selects constant-pattern calls. That is an established ownership alternative to a
  global pattern-history cache. Its compile-time error behavior is not automatically
  correct for Rego: eager compilation could change errors in an unexecuted branch.

D98 currently promises OPA answers, including many exact oracle checks. The live
bad answers are also upstream answers, so changing them must be a deliberate,
precisely documented compatibility deviation: independent regex/template meaning
wins over reproducing an upstream history-dependent defect. It must not be hidden
as a semantics-free performance refactor. This checkpoint records the proposed
correction; it does not adopt one.

## Proposed next experiment, not product adoption

The coordinator requested an isolated comparison of uncached execution against
program/call-site-owned compilation, using actual Program/Machine/builtin seams.
Use explicit compilation identity (ordinary vs template plus delimiter operands),
constant-hot and dynamic/churn inputs, fresh queries and threads, mixed-call cases,
and actual Rust allocations/retention/CPU plus pinned OPA distributions. Lazy
constant-call-site compilation is a candidate to preserve execution-time error
semantics. Dynamic patterns must not create an unbounded retained history.

A query-local cache alone does not establish memory authority. There is currently
no demonstrated heap-budget authority at this cache seam, so moving the whole map
into Context or replacing 100 with another number would leave important obligations
unresolved. Measure the variants before choosing any added state/interface. No
framework, new global manager or fixed replacement cap is justified by this proof.

## R01–R16 accounting and remaining gates

| Rule | Proof / concrete remaining work |
| --- | --- |
| R01 | No production byte changed; assertions/unwraps/cache clearing are test-only. Production allocation and panic audit is not newly certified. |
| R02 | Pinned OPA implementation/module identity, primary semantic declarations/docs, Rust regex and CEL ownership alternatives, live matching outcomes above. |
| R03 | No product literal added/replaced. Experimental cardinalities derive from the existing stated maximum to test its boundary; they do not become policy. |
| R04 | No product state/error protocol change; diagnostic records distinguish actual answers, cold controls, cache ownership and strict failures. Future compilation identity/error lifetime remains to design. |
| R05 | Actual shared keys, context lifetime, per-thread duplication and bypassed retention bound reproduced. Benefit/ownership comparison and production refactor remain unfinished. |
| R06 | Focused diagnostic in existing builtin test module plus one matching upstream probe; no runtime abstraction introduced. |
| R07 | Comments explain diagnostic isolation, exact source checks, context-drop and cold-control purpose. |
| R08 | Pinned OPA compared for correctness/cardinality only; no achieved 4–5× throughput/latency/resource claim; distributions still owed. |
| R09 | Four exact mixed-call cases and retention growth live on both engines before any product edit; compile mistake excluded from proof. |
| R10 | Repeatable run/compare/cases/diagnostic sources, original source hashes/module sum, actual logs/strict statuses retained; no fabricated fix/mutation result. |
| R11 | Native Linux amd64 GNU only. Other six targets and libc/distribution/hardware combinations unrun here; macOS arm64 only and paused Windows ARM VM untouched. |
| R12 | Existing Rego builtin seam; no installed binary/runtime dependency, privilege, authority or crypto change. Go binary is a temporary test oracle only. |
| R13 | Small proof before choosing an implementation; no speculative product framework/cache manager. |
| R14 | Actual compiled-object/cardinality ownership, OPA GC allocation snapshots and process resources. Rust heap/copies/kernel costs/contention/distributions remain unmeasured in this checkpoint. |
| R15 | Scoped recurring lane, no subagents/main merges; coordinator reviews compatibility/evidence and final combined gates. Named-path signed commit and actual validation recorded below. |
| R16 | Private caches/source copies, unchanged admission envelope, held below headroom, no data/evidence/source deletion or active-cache mutation. |

## Validation and evidence artifacts

Kiln r26: library **19 passed; 0 failed; 1 ignored** (the diagnostic was explicitly
run separately); existing `builtins_answer_as_opas_answer` corpus **1 passed;
0 failed**; strict `cargo clippy -p shards-rego --all-targets -- -D warnings`
**exit 0**. Repository policy checker, rustfmt, diff whitespace, shell syntax,
Python syntax, actual cold-control/parity comparison and production-prefix identity
checks passed; see [validation.log](validation.log).

[report.json](report.json) contains the matched live results and allocation
snapshots. [results.tar.gz](results.tar.gz) retains 32 raw logs/statuses/source/module
records, with [results-manifest.json](results-manifest.json) hashes. Expand without
altering the preserved archive:

```sh
mkdir -p docs/research/measurements/rego-cache/results
tar -xzf docs/research/measurements/rego-cache/results.tar.gz -C docs/research/measurements/rego-cache/results
python3 docs/research/measurements/rego-cache/compare.py docs/research/measurements/rego-cache/results/reproduced
```

The recorded source HEAD is canonical c921 with an uncommitted fixture snapshot;
`reproduced/source.sha256` identifies the exact tested fixture/cases/Go diagnostic.
The checkpoint commits that same Rust source. No failing compile is counted as a
semantic red, and no observed upstream-bug parity is counted as semantic correctness.
All jobs are finished at this proof checkpoint; private caches and all raw evidence
remain preserved for the next assigned experiment. No merge to main was made.
