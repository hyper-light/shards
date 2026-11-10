# Closed process-identity failure propagation

Signed source `73c85d039bd72d1867d28fb2b155cee1fef3879e` returns the real held
process reader's capacity failure through public attach as a typed Refusal. Native
GNU/musl focused units and their strict clippy passed; Windows amd64 MSVC strict
cross-clippy passed. Full corpus, native Mac, arm64 and coordinator gates remain
owed. This is not main adoption or a whole-library panic-free/leak-free claim.

## Identity, approved decision and compatibility

POLICY_READ implementer: current main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`; full canonical
AGENTS/R01–R16/evidence/CLAUDE/managed plan/current handoff read. Root selected the
closed producer model after the [exact producer map](records/outer-oom/PRODUCERS.md),
with one simplification: native Os owns operation + authoritative raw code only,
not a redundant decoded kind. Root alone owns audits, research selection,
profiling, benchmarks, heavy/VM/chaos/sweeps, mutations, final compatibility and
main merge. No children or local Mac builds were used.

New branch `p/managed-proc-typed` starts at657474c5, independently descending from
main f5 through the main reader/repro only; it imports no unfinished G22/borrowed
or synchronization ancestor. Owned changes: api taken/lib/client/conformance
precise assertions and these dedicated helpers/records. No dependency, stream,
dial, ownership/stop algorithm, Mutex, wire/tag, credential, grant or crypto change.

The public delta is `Refusal::Process { pid, expected_started, failure }` and
`ProcessFailure`/`ProcessOperation`. Old Gone and all old variants/fields remain;
Refusal retains Clone/PartialEq/Eq without custom lossy implementations. New
Process owns the input scope and direct producer cause. DifferentStart carries
the observed start; Exited comes from the existing held event result. A native
error or UnrepresentedIo is observation failure, not proof of gone/authority.
The [producer map](records/outer-oom/PRODUCERS.md) remains the rationale, with its
proposed Os.kind superseded by the approved code-only state.

This **breaks exhaustive downstream Refusal matches** and changes real native
attach failure matching from Gone to Process. Callers must add a Process arm and
match its exact typed failure/scope; they must not treat every Process/Os failure
as proven gone. The workspace production exhaustive match was Refusal's Display;
native unit/conformance assertions were updated. External typed consumers remain
unknown. Report text now names the operation/raw code and typed cause. No public
Guest/Guests method signature changes; private taken started/exited signatures
now return ProcessFailure. Existing identity/watcher formatting compiles against
its Display and remains explicitly allocating/unproven, not silently fixed.

Os Display renders the code numerically; it does not construct a std I/O error or
decode/store a kind. UnrepresentedIo retains the unexpected no-code kind and
explicitly does not represent an arbitrary custom/static payload. Error::source
transparently delegates to the rendered failure's source, currently None, rather
than returning that same displayed failure again. This follows the
[standard Error contract](https://doc.rust-lang.org/std/error/trait.Error.html#method.source).
The focused test renders a whole source chain into an exact-size stack sink and
asserts exact bytes plus no duplicate source under allocation refusal.

No ErrorKind snapshot claims to represent arbitrary io::Error. Named conversion,
path, buffer, capacity and invalid-record causes are built at their producer,
before String or io::Error::other boxing. Known native code EINTR retains its
existing retry; no-code observations reach UnrepresentedIo. Root approved the
three raw-code guard conditions as **new closed-contract adaptation**, not a
newly reproduced preexisting defect. No mocked reader, hook or guard-mirroring
test was added. A real blocked native read interruption campaign remains owed.

## Exact sources and actual light results

Invocation, with a fresh immutable evidence phase each time:

```sh
bash docs/research/measurements/managed-proc-main/run-typed-light.sh v1
bash docs/research/measurements/managed-proc-main/run-typed-light.sh v2
bash docs/research/measurements/managed-proc-main/run-typed-light.sh v3
```

Private Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, GNU amd64 host,
Linux6.19.10-300.fc44.x86_64, Rust1.98.1 commit
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8. Each concrete source got one
unchanged4CPU/1GiB/2GiB request; all accepted (r22427/32 idle, r2278/32, r22915/32).
No lowered ask, rejected-headroom retry, global toolchain install or cache cleanup.
Kiln's free-GB admission field denotes memory, not disk. No peak-cost claim.

| Job / source | Native GNU | Native musl | Windows amd64 MSVC cross-clippy | Wrapper |
| --- | --- | --- | --- | --- |
| r224 / `2c06343d296c2f0eeb9bfe7758feddd02cbe03db` | build0,9/0 +2private roles ignored, strict clippy0 | build0,9/0 +2private roles ignored, strict clippy0 | 0 | 0 |
| r227 / `b703dbf3a11d0b04a32d1fabd466c6fd49bebecc` | build101/clippy101; tests unrun | build101/clippy101; tests unrun | 0 | 1 |
| r229 / `73c85d039bd72d1867d28fb2b155cee1fef3879e` | build0,9/0 +2private roles ignored, strict clippy0 | build0,9/0 +2private roles ignored, strict clippy0 | 0 | 0 |

r224 is retained as **pre-correction**, not the final chain/unknown contract gate.
Root reviewed its double representation of the displayed failure as Error::source.
b703 delegates source and changes raw EINTR guards; r227 then failed E0433 at
taken.rs215 because removing an io alias left an unqualified native error factory.
73c85d qualifies that Linux reference and the same conditional macOS reference.
The compilation red is preserved; no Linux tests or product runtime failure are
inferred from it. No native Mac compilation result is claimed for that source fix.

The launcher uses a new private target, forces owned source/helper recompilation,
requires selected Cargo executables `fresh:false`, hashes and copies them before
running only `taken::linux::tests:: --test-threads=1 --nocapture`. Cross-clippy
compiles API all-targets and its Windows afunix dependency; it does not run Windows
units or constitute a standalone afunix strict gate. GNU/musl tests use
test-release, never instrumented timing. The parent and all earlier frozen targets
remain untouched. Prior raw record directories were untracked evidence in later
checkout status, not altered Rust sources; per-run selected hashes match Git blobs.

Private checkout:
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-proc-typed-7df18a17fa`.
Private target: `~/managed-proc-typed-target`. Immutable evidence/executables:
`~/managed-proc-typed-evidence/{v1,v2,v3}/`; binaries were copied before each test.

## What the final focused tests establish

The earlier [real caller red](records/outer-oom/README.md) at6db is still immutable:
valid own TGID/start, after0, real Held::find aborted134 on28B; no return marker.
At exact73, both GNU and musl now return CapacityUnavailable from that same real
call under after0, with actual parent FDs9→9. The public `Shards.attach` early path
returns Process with that same PID/expected start/cause under after0, FDs9→9,
before its later success Box/Arc/thread/path setup. All test logging/assertion
formatting is outside the refusal interval.

For wrong-start/exit, one owned Child is held by its actual pidfd before sampling
start. While alive, wrong expected start returns DifferentStart with the exact
sampled original value, both internally and through public attach. The parent
explicitly closes its inherited stdin writer; the held pidfd exit event is awaited
while the Child remains **unreaped**. Actual common Held::find and public attach
then return Exited for that same PID/start. Neither result is inferred from an OS
error. Only afterwards the parent waits exit0, drops the held object and reports
cleanup before final assertions. RAII owns Child kill/wait on errors; no PID-only
signal, detached child, timer/poll loop or new generic fixture manager.

Both libcs record wrong-case FDs11→11, exited-case10→10, final9 matching initial9;
child exit0. These are bounded observations of these cases, not a global leak or
kernel accounting proof. Existing native attach-after-stop conformance checks
accept exact scoped ESRCH at pidfd-open/exit-registration only **after** their
independent owned stop/wait fact; generic OS/Process failure is not accepted as gone.
Those existing full-corpus cases compile here but were not executed by this filter.

Other ordinary tests preserve raw/nonUTF8 comm and name restoration/child0/reader
join, PID/path and decimal field boundaries, typed actual reserve failure after0/1,
empty/byte/error reads, conversion refusal, and exact Clone/Eq/
chained Display/no-code uncertainty behavior under refusal. Only two private child
roles are ignored; the ordinary tests actually invoke them. No normal regression
or OOM case was skipped to obtain these results.

## Artifacts and remaining limits

| Record | GNU executable SHA256 | Musl executable SHA256 | Original raw archive SHA256 |
| --- | --- | --- | --- |
| [v1](records/typed-v1/) | `31d08615963accc54caf1af9b72be2211cccdf3e74984dc97a340a6bfbcc77b5` | `06d3d16b598c5e0e5a7b9143e30483edc38866246b07eaa3af0c08cb40323ccd` | `cdeca9be12977cfb3259308930ccfd8f5512701ba142a23b5a3a2ff709f08774` |
| [v2 compile red](records/typed-v2/) | no selected executable | no selected executable | `cb1b95c5cf650dfa241d8f9b6ff30452f3eee12e18d9b1a9f5dbb5a59068f549` |
| [v3](records/typed-v3/) | `040c3366f0adb3973870d0c106b800136d7e7e5844ff01b0a835c2e541be9c11` | `6c251977ff9bb883dbf6aa77c28dd7e1d58207e3acfe33c99d5e09b5a3234241` | `f30c946125bccf5699c0d52fda74e0bfb63316b68f5b71de2beeb8607efc3296` |

Original archives, checked regular-file extractions, admissions, compiler JSON,
commands/exits, status/toolchain and selected input hashes are retained. No binary
is committed. Historical parent manifests remain historical; each new phase has
its own evidence manifest. Original raw libtest whitespace is preserved.
Source-only diff/format/shell/policy checks passed. The staged whole-evidence
whitespace check returns2 solely for final blank lines in four original raw
GNU/musl libtest logs; [the exact result](records/typed-whitespace-check.json) is
retained, and their bytes still match the original archives.

| Rule | Actual proof and remaining gap |
| --- | --- |
| R01 | Real Held/public attach after0 return typed cause; FD returns and presentation/traits under refusal observed. Native conversion/capacity/parser errors no longer construct Strings/boxed errors at this seam. Legacy identity/watcher/conformance reporters, success allocations and whole Mac owned Mutex64B abort remain unproven/open. No whole-library claim. |
| R02 | Closed producer/source map and alternatives reviewed/selected by root; Os simplification and Error source correction documented. Full research/error audit remains root-owned. |
| R03 | No production capacity/default/timer constant added. Existing stack path/field/radix derivations preserved. Raw OS code is authoritative; no redundant decoded-kind cache or budget enlargement. |
| R04 | Scope and exact producer/operation typed; mismatch/exit distinct from observation failure. Unknown no-code payload explicitly unrepresented. No errno-only authority/terminal decision. |
| R05 | Existing internal process-identity seam/shared implementation; no framework/cache/manager/dependency/Box/String bridge. Same native event waits; root actual interruption campaign owed. |
| R06 | Direct producer/error cases and one fixed-boundary I/O conversion; strict all-target API clippy0 for three selected triples. Broader audit root-owned. |
| R07 | Native safety comments retained; comments explain fixed-boundary conversion, uncertainty and unreaped-child identity. |
| R08 | No speed/4–5×/allocation-free/leak-free claim. Exact-source competing performance data remain root-owned. |
| R09 | Exact real outer abort retained before fix. Error-chain and raw EINTR changes are reviewed new-contract corrections, not newly reproduced preexisting bugs. |
| R10 | Red6db, pre-correction2c green, b703 compile red and final73 green retained with exact artifacts. Root mutation/restoration proof still owed. |
| R11 | GNU/musl amd64 native focus and Windows amd64 MSVC cross-clippy only. Mac arm64, Linux arm64 libcs, Windows arm64/native and seven-target/VM checks unrun. Intel Mac unsupported; paused Windows ARM untouched. |
| R12 | Same Unix held identity module with smallest existing native adapters. No new installed binary, hostroot, crypto, dependency or parallel process manager. |
| R13 | Closed values at real producer branches, private signatures migrated for actual identity/watcher callers, no reverse error framework. |
| R14 | No implementer profiling/benchmarking; syscall/CPU/alloc/realloc/retention/fault/parent+kernel comparison owed to root. FD unit observations are not complete resource-accounting proof. |
| R15 | Scoped source/light only, signed named-path commits/trailer, no children/mutations/heavy/main merge. Root owns review/adoption/combined gates. |
| R16 | New private target/evidence, old6db/root targets untouched, no deletion or headroom bypass; exact unchanged admission for each source. No local Mac build. |

Root owed: full ordinary API/conformance corpus on reviewed source, actual blocked
native read interruption, independent guard removals/restoration and refusal
propagation/error-kind/cleanup controls, native Mac compile/light and supported
seven-target/architecture/libc matrix, relevant real VM compatibility, exact-source
costs and parent/kernel resources, external exhaustive-match migration review and
final main composition. Historical owned-Mac synchronization RED and the separate
cmdline reporting defect are not fixed or causally combined by this checkpoint.

Before this evidence-only commit, root's independent full ordinary corpus at
main compositionbf2535/r345 reported lib40passed/2failed/5ignored and
sim15passed/0failed/2ignored. The two failed client attach-after-stop assertions
accepted only exact ESRCH at pidfd-open/exit-registration, but the correct actual
typed result was matching PID/start with Exited after the independent stop/wait.
This is a fixture assertion defect, not a production observation failure. Root
requested a subsequent two-assertion-only correction and owns the full rerun;
no whole-corpus or main readiness claim is made here. Root raw log remains
`~/root-main-native-api-bf25-v1/lib.log`; its clone/target are not modified.
