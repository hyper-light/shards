# On-demand cgroup Reader light tests

Implementer scope: NEW `crates/init/src/metrics/tests.rs` and this evidence
directory only, based on main `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`.
Policy `docs/development-rules.md` SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
AGENTS, full R01–R16, development-evidence, CLAUDE, managed plan and updated
`/Users/adalundhe/g121-handoff.md` were read before implementation and reread
after compaction. ROOT owns the reader, registration, integration, audits,
collector validation, profiling, benchmarking, heavy gates and main merge.
The completed `80cabe316`/`f408e66e` capture worktree remains untouched.

## Actual seam and inputs

All ten tests call actual `Reader::new()` and
`Reader::snapshot(&held_directory_file, Scope)`. Their 24 snapshots open and read
named private regular files through the production `openat`/`File` implementation.
The fixture uses the existing `shards_testdir::TempDir`, mode 0700, holds its
directory `File`, closes it before dropping the directory, and asserts that the
owned directory has disappeared after each snapshot. File close return codes,
kernel allocations and process-tree resource return are not observed.

There is no alternative parser, mocked reader, fabricated counter producer or
new repository dependency. ROOT will add `#[cfg(test)] mod tests;` to its reader.
The private standalone harness imports the same leaf and reexports the real
types at the parent boundary; it does not amend the production source. A minimal
private Cargo workspace avoids a full guest/workspace build.

Frozen source association:

| Input | SHA-256 | Origin |
| --- | --- | --- |
| Reader | `8f35e059201b73618fa23cfd3f33a394253a701cc9520e267950513873e9f1e5` | ROOT's then-uncommitted `managed-probe-c141/crates/init/src/metrics.rs`, 8,945 bytes |
| Codec/types | `9aa16e1df9b84c4cad69744e50a44f93be66d83afd59642304d010d0b85de354` | signed `11d1c04d27a52133f3447321fd4b7d1057ce0212`, 16,484 bytes |
| Test leaf | `5925769c60afa97d07b3e6940b8aa0d31fc35bbcbc46d97181aed06f93888a63` | this checkpoint, 8,540 bytes |

The initial reader inspection saw `64d8a303...`; before copying, the current
ROOT file was reread in full and hashed as `8f35e059...`. Tests bind to the latter.
Codec follow-up `d5000a168` changes an external restore-count label and is not
silently substituted into these frozen inputs. No encoder assertions or collector
results are claimed here. Exact source maps and all copied helper/config bytes
are retained in the input archives; no maintained parser copy is introduced.

## Coverage and limits

| Test | Observable contract |
| --- | --- |
| selected fields/scopes | unordered device/field order, unknown fields, EOF without newline, exact sums; Run/Workload/Member identity preserved |
| empty/missing IO | present empty file yields actual zero; absent file yields both typed `Io(ENOENT)` values; independent metrics survive |
| missing usage | absent CPU key yields `Missing`, while memory, tasks and IO remain known |
| per-device duplicates | duplicate read or write field yields `Invalid` |
| IO overflow | selected number overflow and checked read/write sums yield `Overflow` |
| decimal bounds | full `u64::MAX` survives; `u64::MAX + 1` derived in u128 yields typed overflow |
| scalar tokens | additional scalar tokens across spaces or newlines yield `Invalid` |
| CPU page boundary | selected key and number straddle the actual page size queried through `sysconf` |
| IO page boundary | selected read and write tokens straddle that same runtime-derived boundary |
| long unknown fields | unknown keys/values two actual pages long do not hide selected metrics |

Page crossings place data across the reader's actual buffer-size boundary. They
do not trace every native read length or prove that regular-file reads are always
full. Long-field tests establish observable selection, not allocation counts or
retained-memory bounds. Literal counter values are synthetic test data; scopes,
decimal boundaries and file names come from the actual interface. No new runtime
limit or production constant is introduced.

These are **controlled regular files, not privileged real cgroups**. There is no
live-kernel controller, cgroup ownership, hierarchy charge, collector, OTLP wire,
VM, lifecycle, failure-injection or performance proof. An unexpected fixture
result would be a source-specific parser observation, not a reproduced live
kernel defect. No production correction is part of this batch.
The reader opens the files sequentially; its timestamp does not establish an
atomic cross-controller kernel snapshot.

## Sources and alternatives

The [Linux cgroup-v2 interface](https://docs.kernel.org/admin-guide/cgroup-v2.html)
specifies flat-keyed CPU stats, memory in bytes, task/TID accounting and unordered
per-device IO byte fields. The tests select those named fields rather than
depending on positions. [openat(2)](https://man7.org/linux/man-pages/man2/openat.2.html)
defines relative resolution through the held directory descriptor;
[sysconf(3)](https://man7.org/linux/man-pages/man3/sysconf.3.html) supplies the
running process's page size. These primary sources were opened on 2026-11-10.
The [Rust destructor contract](https://doc.rust-lang.org/reference/destructors.html)
specifies declaration-order field destruction, which is why the held File
precedes TempDir. The [Read contract](https://doc.rust-lang.org/std/io/trait.Read.html)
permits shorter reads; the page tests therefore make no full-read trace claim.

A copied pure parser would miss real File/openat errors and duplicate maintained
logic. A full init build or privileged cgroup fixture would add unrelated guest
and controller requirements to this ordinary unit-test task. The chosen seam
uses the production reader and existing test-directory helper directly; broader
kernel/collector evidence remains ROOT's work.

## Reproduction and operational envelope

`prepare.py REPOSITORY READER CODEC NEW_OUTPUT` refuses mismatched reader/codec
hashes, copies sources unchanged and records each input. It copies canonical
rustfmt/clippy/toolchain configuration and pins libc to canonical Cargo.lock's
`0.2.189`. It introduces no dependency in the repository. Existing testdir source
and manifest remain byte-identical. Archived frozen sources can be supplied as
READER/CODEC after ROOT moves its live files.

The owned Linux scope is admitted through Kiln with `linux/amd64 cpus=2 mem=1g
disk=2g`, `sync:false`, `reuse:false`, no root/protected cache. The runner guard's
admission is a reservation/headroom check, not a claim of measured process peaks.
`RUSTUP_TOOLCHAIN=1.98.1 python3 run-light.py SCOPE` executes only tool identity,
format, these Reader tests and strict standalone Clippy. Toolchain 1.98.1 was
already installed; no installation is requested. All commands share one original
240-second monotonic deadline; expired owned command groups are killed and joined
within 15 seconds, with explicit unknown if joining fails. The outer Kiln bound
is 300 seconds. Caller limits are operational test budgets, not learned product
limits or performance measurements.

Source, target, Cargo home, TMPDIR, XDG and Shards cache all point inside the new
owned scope. TMPDIR containment keeps the existing testdir stale-PID sweeper
away from unrelated experiments. The helper's existing per-process descriptor
soft-limit initialization remains present; no host/admin setting is changed.
No helper binary is invoked, no network socket or worker is created by the leaf,
and no sleeps, probes, polling, retry loop, warmup or cache workaround is added.
Normal Cargo dependency retrieval may access the registry in that private scope.

## Results

Results and raw artifact verification are recorded in `RESULTS.md`. The first
format attempt was an actual red: the private package omitted canonical
`rustfmt.toml`, so default width produced diffs against all unchanged input files.
It stopped before compilation/tests. The packaging correction copies that config;
it does not normalize or modify frozen source. The original input and raw red are
preserved rather than relabelled as a production failure or passing test.

## R01–R16 accounting

| Rule | Applicable evidence and remaining limit |
| --- | --- |
| R01 | assertions/unwrap are test-only; no production edits; actual reader panic/allocation audit belongs to ROOT |
| R02 | primary kernel/man-page contracts and existing TempDir inspected; shared real reader chosen over a copied parser or broader privileged fixture |
| R03 | page and decimal limits derived from sysconf/u64; synthetic counter literals are test data; caller budget explicitly operational |
| R04 | actual typed Result/Failure/Scope exercised; no new production lifecycle state |
| R05 | one actual Reader and held directory per fixture; buffer reuse remains real source behavior; no new cache, wait, manager or polling |
| R06 | focused file fixture plus ten independent ordinary tests; broader complexity audit unrun |
| R07 | comments cover regular-file limitation, ownership/drop order and unsafe sysconf obligation |
| R08 | no comparative or 4–5x claim; all baseline measurements owed to ROOT |
| R09 | no speculative production fix or live-kernel defect claim; harness format red retained |
| R10 | exact source maps, caller, frozen input and raw result artifacts retained |
| R11 | assigned native Linux GNU light result only; other architectures/libcs and complete supported matrix unverified here |
| R12 | existing Reader/OTLP types and testdir shared; no platform abstraction or copied parser introduced |
| R13 | new test leaf/evidence only; parent registration and production source stay with ROOT |
| R14 | no allocations/faults/peak/leak/resource-performance claim; profiles and benchmarks ROOT-only |
| R15 | implementer ordinary light scope, signed named paths; no children/goals/campaigns/main merge; ROOT owns final integration and audit |
| R16 | fresh admitted private scope, preserved old proof worktree and all raw artifacts; no protected cache or paused VM touched; no deletion |

Owed to ROOT: compose registration with exact production Reader and selected codec,
normal init/guest compilation, supported-target checks, real held-cgroup observations
and real collector validation, independent audit and required performance work.
Passing this leaf cannot close those obligations or the full managed-runtime plan.
