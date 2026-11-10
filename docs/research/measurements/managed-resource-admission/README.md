# Existing-cap pre-launch resource admission

Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Base/main
`cbd39284937d024640c56db6f8d8ef2bd9b0ee24`, branch
`p/managed-resource-admission-v1` in the existing sparse checkout. Frozen startup
work remains on `p/managed-allocation-metrics` at `89dec877a` and is unchanged.

Full canonical AGENTS, rules R01–R16, development-evidence, CLAUDE, current
managed-runtime-plan and global handoff were read before edits. Rules SHA256:
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
POLICY_READ key `managed-resource-admission-policy-read-cbd392849-v1`.
The direct current task/new policy override historical role/hash text in the
global handoff. This lane remains implementation/light only; no children/goals.

## Actual production path

Latest follow-up source `5f131376ec43681ff50d32b9c1ad0a9f3ec6adf7` adds a
checked diagnostic writer: every fragment fits actual remaining String capacity
before push; formatting failure discards partial output. Build0, nine pure/private
units0 and isolated Clippy0, plus actual full init binary-target cargo check0.
The latter emits one obsolete `run::workload_headroom` warning; ROOT owns that
unassigned site, so actual full-caller Clippy is not claimed. No linking/helper/
init/controller/VM execution follows from this typecheck. Exact source, commands,
capacity-fault unit and original archives are in `records/caps-v4/`,
`records/caller-v2/` and `records/formatter-proof.json`.

[P0 authority trace](AUTHORITY.md) includes the exact public cap/self-limit
contract at AGENTFILE_ARCH.md:885–889 and921–923, the real run/workload/ROOM
producers, concrete capability prototype and explicit absence of a checked
guaranteed-claim producer. ROOT owns that producer/normalization decision; this
checkpoint adds no guarantee hook, flag or ledger.

`domains::start` now calls the new `domains/admission.rs`: hold the existing run
directory, verify cgroup2 and required controllers, enable/read back the domain
subtree, then apply/read back the existing aggregate pressure and swap settings.
Each fresh member enters `Prepared`; only successful cap/OOM/control verification
returns `Committed`. The original server and actor calls follow that result.
The actor's original clone uses that held directory; successful clone transfers
its descriptor into the existing lifecycle owner. There is no new manager or
installed binary, and all host backends use this one Linux guest implementation.

Control operations use fixed names with openat relative to held objects, not a
cached mutable path. Existing asks stay caps: `Some(0)` and absent/`max` remain
different. No equal shares, sum-cap rejection, CPU/IO/storage policy or D59 victim
change is introduced. Fresh unused resource-preparation failures attempt only
rmdir of that component and retain the rollback result/errno. Existing directories
are never removed by this ownership guard; no recursive cleanup exists.

Later preparation unwinding also attempts unused removal. A Drop cleanup failure
is not a witnessed removal, especially after the existing server has populated
the group. Root's server/RUN/SCM_RIGHTS/effect-spawn integration remains separate;
this change does not establish its transfer/cleanup/capacity contract.

Typed refusal records retain cause, fixed control name, expected/observed values
and rollback state. The existing `start` String boundary is presentation only;
new rendering reserves its exact size fallibly. On diagnostic allocation failure
the outer Err can have empty text, and no actor is released as a result. No caller
may infer retry, authority or success from that text; a wider typed startup API is
not claimed here.

## Primary-source basis and alternatives

Linux v6.18 distinguishes overcommittable caps from allocations and protections;
controller availability and subtree enablement are hierarchical. `memory.high`
is pressure control. `memory.min` protection is bounded by ancestors and can be
diluted by overcommit; `memory.low` is best effort. None of those observations
establishes an owner reservation. [Pinned cgroup v2 contract](https://raw.githubusercontent.com/torvalds/linux/refs/tags/v6.18/Documentation/admin-guide/cgroup-v2.rst).

The general memory-interface wording permits page rounding; this implementation
uses the pinned implementation's actual floor and sentinel rules:
`page_counter_memparse` divides bytes by PAGE_SIZE and clamps to PAGE_COUNTER_MAX;
`memory_max_write` uses it, and the tunable renderer emits `max` at the sentinel.
The guest page size comes from sysconf, not an architecture guess. Both fractional
page and sentinel behavior still require actual guest confirmation at root's
chosen kernel. [Parser](https://raw.githubusercontent.com/torvalds/linux/v6.18/mm/page_counter.c),
[bound](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/linux/page_counter.h),
[write/read implementation](https://raw.githubusercontent.com/torvalds/linux/refs/tags/v6.18/mm/memcontrol.c).

runc v1.3.3 pins opencontainers/cgroups v0.0.4. That implementation checks
available controllers, cleans newly created path elements on errors, and lets
resource setters reject unsupported limits. It does not establish a hard owner
reservation from a successful write. This implementation instead requires exact
readback for the controls the existing Shards caller requires.
[Pinned dependency](https://raw.githubusercontent.com/opencontainers/runc/v1.3.3/go.mod),
[creation](https://raw.githubusercontent.com/opencontainers/cgroups/v0.0.4/fs2/create.go),
[pids](https://raw.githubusercontent.com/opencontainers/cgroups/v0.0.4/fs2/pids.go).

systemd v258 enables controllers through its hierarchy and reports effective
memory/task maxima. Its `MemoryMin`/`MemoryLow` protection settings are distinct
from `MemoryMax` and `TasksMax`; their existence does not authorize Shards to
invent guaranteed shares. [Pinned systemd contract](https://raw.githubusercontent.com/systemd/systemd/v258/man/systemd.resource-control.xml).
Exact downloaded bytes/hashes and HTTP429/404/time-out attempts are retained in
`research/` and the source/tool records; unavailable retrieval is not success.

Repository evidence: D59 parts one/nine and the existing `Domain::{pids,memory}`
parser define the caps. `run::cgroups`, `run::RUN_CGROUP`, workload `memory.max`
and `memory.current`, plus `plug::{unreported,still_pluggable}` define the current
hierarchy and pressure calculation. The retained
`managed-runtime-first-runs/README.md` already records that MCP tasks share pids
budgets; no new helper task allowance is invented here.

## Validation status and exact remaining boundaries

Eight tests now PASS on source `bc381c013d4a53bebb94efe4df2289c34acc6380`:
scalar/overflow, kernel rounding,
controller tokens/read errors, checked pressure arithmetic, missing/malformed/
mismatched/symlink controls, fresh-only rollback, descriptor transfer and names.
Ordinary private files exercise IO/state behavior; they do not emulate cgroup
charging, kernel rounding, population or admission. Test TMPDIR/Cargo home/target
are private and serial. No test invokes the real run directory or controllers.

Local source formatting/checks and `scripts/check-development-policy` pass.
Actual formatter: rustfmt1.9.0-stable/48a229ce; local Rust/Cargo1.98.1, recorded
as observed versions, not an enforced toolchain pin. No local compilation ran.

The first remote light invocation was rejected before start: Fedora8f had zero
idle CPUs. No job, directories, executable or unit/Clippy result exists from it.
Exact attempted command/source package/result are retained in `records/`.
Fresh inventory also had no admitted Linux runner. Admission remains
1CPU/1GiB RAM/2GiB disk, serial/sync:false/new private paths. No guard bypass,
larger envelope, local fallback, root-cache write or cleanup occurred.

After that initial hold, v1 was admitted but its narrowed workspace failed before
Rust compilation because unrelated vendor patch paths were absent. V2 corrected
only the private workspace adaptation: build0/eight units0, then Clippy101 for a
misplaced descriptor safety comment. Source bc381 moves the same ownership
conversion under its adjacent safety explanation. V3 build0, eight units passed
with zero failures/filters, and Clippy0. Production dependency/crypto/provider
configuration is unchanged. The isolated import's only allowance is dead_code;
new production panic/unsafe checks remain enabled.

All three original archives and their12/19/19 regular members were transferred
and byte/hash verified in `records/caps-v{1,2,3}`. Actual remote Rust1.98.1
commit48a229ceaefd4985c50990b14116b6d856af0985/LLVM22.1.8 and
Cargo1.98.1 commit797e8a9bca276c1c9f9f738d2a20f484fa4eea9d are retained.
The V3 test executable is
`/home/adalundhe/managed-resource-admission-target-cbd392849-caps-v3/target/debug/deps/managed_resource_admission_light-af49216697ca4c33`,
7,430,344B, SHA256
`ba958429fdea359f05751ba43b7a5cec916d9b4ae7a410b9db1916a6059f062b`.
It was hashed before execution and reverified after Clippy and during collection.
All own jobs have completed; private sources, Cargo caches, binaries and raw
records remain preserved. No actual run-group/controller/full-caller/VM operation
was executed by the unit harness. Exact commands/environments are in each record.

ROOT owes actual full init/helper caller compilation, live cgroup/kernel-rounded
readback and refusal/unused rollback/FD/identity/lifecycle/cleanup cases, bounded
mutation/restoration, host/backend/target integration and all profiling,
attribution and benchmarks. This is a new admission requirement; source
observations about earlier behavior are not reproduced defects or measured gains.

## Continuing hierarchical resources: actual caller and authority boundary

The next actual caller is still `domains::start`, before its existing server
and `managed::Launch`, with root owning server/RUN transfer integration. The
current domain `asks` parser exposes no memory guarantee: its numbers are caps.
The primary workload's remaining `-m` cap is used by D59's pressure policy; the
new read is fallible, but separate usage/max/free/hotplug samples are not an
atomic reservation. Existing hotplug fallback behavior is not upgraded into a
known parent guarantee. The domains parent has `memory.high`, not a hard sum
reservation, and its largest-domain victim policy is unchanged.

The minimal next lease seam must take explicit owner authority, affected
run/member/execution/flow identity and an explicit Caps versus Guaranteed request;
`prepare` returns an owned pre-release charge or typed Unsupported/Unadmitted,
`commit` transfers it to the actual lifetime, and release follows owned cleanup.
No generic unused ledger is implemented in this cap batch. The continuing task
must trace actual owner bounds and protection/reclaim semantics before admitting
any guarantee. Missing hard support/authority must refuse strict mode before
release. No invented percentages, equal shares, cached capacity or sum-cap
refusal may substitute. Protected control and induced parent/kernel charges,
execution/flow resource owners and the strict-mode declaration/actual caller
remain OPEN production work, not established by these readbacks.

## R01–R16 account

| Rule | Evidence / remaining limit |
| --- | --- |
| R01 | New control/name/format allocation paths are checked/fallible; tests may assert. Typed error remains Err even if outer diagnostic allocation fails. Full native/caller/OOM gates owed. |
| R02 | Pinned Linux, runc/cgroups and systemd plus current real caller/D59 inspected; exact sources and failures retained. |
| R03 | Caller caps/pressure values retained; decimal buffer derives from u64 grammar, page size from kernel, mode matches existing mkdir. No measured number becomes a policy literal. |
| R04 | Prepared→Committed/Refused and unused/retained rollback ownership explicit; expected/observed readback typed. Later Drop cleanup not a success witness. |
| R05 | Existing caller/control files and owned descriptors; no cache, periodic probe, tracing or parallel manager. |
| R06/R07/R13 | Narrow domains resource sites, focused new module, unsafe obligations documented. Root review/full caller proof pending. |
| R08/R14 | No resource/performance/competitive claim. Parent/kernel/foreign costs and paired profiles remain root-owned and unrun. |
| R09/R10 | New requirement, not an unreproduced defect fix. Actual pre-start admission failure retained; kernel red/green/mutation still owed. |
| R11/R12 | One Linux guest implementation on both guest arches/all host backends. Linux compile/light pending; seven targets and actual KVM/HVF/WHP unverified. ARM Windows paused. |
| R15 | Implementer scope only; no children/goals/main merge. Root owns integration/audits/profiling/benchmarking and assigned actual gates. |
| R16 | Local below30GiB reported; no local build/large copy/cache deletion. No active own local target; frozen source/evidence/binaries preserved. |

## Coordinator artifact storage

The combined staged whitespace gate rejected original upstream trailing spaces and three raw libtest EOF blank lines. `coordinator-raw-storage.json` records the four original and compressed hashes. Those exact files now have a `.gz` suffix; `gzip -dc` recovers their original bytes. Existing raw archive contents and source behavior are unchanged. This corrects evidence storage, not upstream source or native outcomes.
