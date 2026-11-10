# Parent resource authority: existing controls and the missing entitlement producer

Research for ROOT's P0/P1/G10–G12 decision, 2026-11-10. **Current Shards supplies
configured ceilings, pressure policy, placement and observed usage; the inspected
owner/run/member inputs do not supply a checked guaranteed-allocation claim.**
Neither an artifact ask nor an observed remainder fills that missing input.
This note proposes no public parameter, manifest field, equal split, topology,
ledger, new controller write or D59 victim-policy change.

## Scope, identity and method

Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Own **only this new file**. The frozen Rust
reference remains at signed `8338bf864e1dfc12a45de4c7f22d0a9e9535ec84`;
ROOT owns its measurements. No active source/campaign was rebased or rebuilt.

POLICY_READ: current main `a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`, full
AGENTS, R01–R16, development-evidence, CLAUDE, managed-runtime-plan and
`/Users/adalundhe/g121-handoff.md`; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
The newer user/main receipt supersedes stale revision/status text in that handoff.
This is bounded primary-source research, not a management-layer audit or a live
capacity/correctness result. No VM, controller operation, collector, campaign,
build, benchmark, profile, child agent, root-cache access or main merge ran.

Source reads began at ROOT candidate `cf8f8d600414aced7b24ddfea10410f8ddafd103`.
A targeted `git diff --name-only cf8f8d600414aced7b24ddfea10410f8ddafd103
a920c85b5c0c9d40aafc4f4cce32432c7c0aed79 -- <the source paths below>` was empty.
The trace therefore cites the unchanged current-main bytes, not a moving branch.
Method: `git show REV:PATH`, bounded symbol searches/line reads, official HTTPS
sources at resolved commit IDs, and SHA-256 over retrieved bytes. Tag resolutions
used GitHub's `git/ref/tags/TAG`, following annotated tag objects to their commit.
No assertion is made that upstream Linux alone identifies a deployed Shards kernel,
its patches, configuration, page size, hierarchy mount options or controller state.

## Keep four different facts separate

| Fact | What would establish it | What is insufficient |
| --- | --- | --- |
| Configured ceiling | An authorized request plus successful configuration/readback at the identified scope, subject to ancestors and controller semantics. | A promise that the scope can consume the ceiling, or addition of children's maxima as reserved rights. |
| Protection | The relevant protection mechanism and its actual effective hierarchy; distinguish hard reclaim protection from best effort. | The name `reservation`, a nonzero requested minimum, or a child readback alone. |
| Admitted right | An explicit owner-authorized promise, a parent able to honor it, serialized admission/transfer/release and a defined failure/revocation policy. | A logical count, a borrowed label, free-memory sample, ROOM answer or artifact-supplied requirement. |
| Charge | The resource actually acquired/retained for a producer, including work induced in supervisors and kernels, with an identified lifetime. | Child RSS alone, an observation label, or moving a process after it allocated. |

The last two rows are requirements/inferences from the
[managed plan §5](../design/managed-runtime-plan.md#5-resources-delegation-and-control-survival),
not claims that the missing admission mechanism already exists.

## Primary interfaces and their actual promises

### Linux cgroup v2

The kernel defines separate distribution models: limits may be overcommitted,
protections may be overcommitted and then share the parent's effective protection,
whereas exclusive allocations cannot be overcommitted. A sum-of-child-maxima
test would therefore change limit semantics; it does not follow from the
allocation model merely because both use numbers.
[Linux v6.18 model definitions][kernel-models]

| Interface | Meaning relevant to this decision | Boundary |
| --- | --- | --- |
| `memory.max` | Charge ceiling with reclaim/OOM handling. A limit update can itself reclaim or invoke memcg OOM; nonblocking update differs. | Successful write/readback does not allocate the requested backing or prevent an ancestor/global failure. [Implementation][memcontrol-max] |
| `memory.high` | Pressure/throttle control rather than an exclusive allocation. | Reaching it does not by itself select a guaranteed victim or confer extra capacity. [Kernel interface][kernel-memory] |
| `memory.min` / `memory.low` | Min is hard protection against reclaim within the effective boundary; low is weaker and can yield under pressure. Effective values depend on the reclaim hierarchy, actual protected usage and sibling overcommit. | A configured minimum is not preallocated unused RAM. The implementation redistributes unused protection and computes effective protection during reclaim. [Protection calculation][page-protection] |
| `pids.max` / `pids.current` | Hierarchical task admission limit and current subtree count; fork can fail with EAGAIN. | A cap reserves no future tasks. Existing population can exceed a lowered cap or organizational migration; parent constraints still apply. [Controller implementation][pids] |
| `cgroup.controllers` / `cgroup.subtree_control` | Available and enabled controller authority down a delegated hierarchy. | A child's controls cannot create resources or authority absent above it. [Availability/delegation contract][kernel-controllers] |

Units and kernel normalization matter. `page_counter_memparse` divides bytes by
the actual PAGE_SIZE and clamps to PAGE_COUNTER_MAX; `memory_max_write` uses it.
Do not reject an otherwise supported cap solely because requested bytes differ
from the kernel-normalized readback. Shards' current admission already models
that floor/sentinel behavior; no change is proposed here.
[Parser][page-parser], [consumer][memcontrol-max], [current admission][shards-admission]

Memory ownership follows instantiation and persists until release; migrating a
process does not transfer its old charges, and shared use does not identify one
semantic producer. Thus a member tag is not kernel cost attribution.
[Memory ownership][kernel-ownership]
The kernel's kmem charging path also uses an object cgroup, not a Shards execution
ID. Which particular shared buffers/objects are charged where in the real Shards
caller remains an actual attribution question for ROOT.
[Kmem charge path][memcontrol-kmem]

### systemd v258

| Existing interface | What it exposes |
| --- | --- |
| `MemoryMax`, `MemoryHigh`, `TasksMax`; `EffectiveMemoryMax`, `EffectiveMemoryHigh`, `EffectiveTasksMax` | Configuration and effective constraints. Effective memory max considers ancestor slices and physical memory; it is not an available/reserved-byte lease. [Resource settings][systemd-resource] |
| `MemoryMin`, `MemoryLow` and child defaults | Protection settings, with ancestor configuration and shared undistributed protection. Shared protection leaves siblings competing; the property's use of “allocation” does not change the kernel model into exclusive backing. [Resource settings][systemd-resource] |
| `SetUnitProperties(name, runtime, properties)`; `StartTransientUnit(name, mode, properties, aux)` | Modify supported properties or create/start a transient unit with them. They configure a named unit; the cited API does not mint Shards owner/member entitlements. [D-Bus contract][systemd-bus] |
| `Delegate=` | A management boundary: systemd owns the unit itself and leaves delegated descendants to their writer. Follow the single-writer rule. Delegation of management is distinct from a memory allocation. [Delegation contract][systemd-delegation] |

These are researched alternatives/interfaces, not instructions to modify a host's
service manager. This lane made no D-Bus call or host-controller change.

### OCI runtime-spec v1.3.0 and the established runc mapping

OCI `linux.resources.memory.limit` is a limit; `memory.reservation` is a **soft**
limit. `pids.limit` is a task maximum, including valid zero and `-1` for unbounded.
The `unified` map addresses v2 files and requires errors for required controllers
that are absent or cannot be enabled. A general configuration map is not an
owner-guarantee issuer.
[OCI memory, pids and unified contract][oci]

runc v1.3.3 pins `opencontainers/cgroups` v0.0.4 and runtime-spec v1.2.1; it is
not claimed to implement the separately inspected v1.3.0 in full.
[Actual dependency pins][runc-mod]
That cgroups implementation writes `Memory` to `memory.max` and
`MemoryReservation` to `memory.low`. This is concrete evidence against interpreting
the established reservation field as reserved `memory.min` or backing capacity.
[Actual v2 setter][runc-memory]

## Exact Shards owner → run → member trace

All source links in this section are fixed at main
`a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`. These are bounded source facts,
not exhaustive absence proofs or newly executed acceptance tests.

| Real boundary / exact path and symbol | Existing input/effect | What remains unestablished |
| --- | --- | --- |
| [`crates/ipc/src/lib.rs:1130–1176`, `Resources`][shards-resources-input] | Owner run resource fields include `memory`, `memory_reservation`, `pids_limit`, CPU/IO caps. | No checked owner-guarantee claim in this structure. Existing fields retain their meanings. |
| [`crates/shards/src/run.rs:388–405,978–1004`, `Prepared` / `sizes_for`][shards-run] | Parent selector; guest boot/max sizes derive from run resource limits and observed host memory. | VM addressability/hotplug maximum is not exclusive host backing. |
| [`crates/shards/src/daemon/parents.rs:17–64,122–157,201–235`, `check/group/place`][shards-parents]; [`daemon.rs:8070–8115`, `group_of/start`][shards-parent-call] | On Linux, select the user's systemd slice or delegated path; place a spawner before it creates VM processes. The default uses the daemon's place. Current macOS/Windows have their existing Shards-group seam. | Placement identifies where costs fall; neither the selector nor a successful placement promises new entitlement. No new host backend/topology is needed for the shared Linux-guest cap path. |
| [`crates/shards/src/daemon/budget.rs:25–178,503–508,829–850`, `room/Holding/held_at/cgroup_of_parent`][shards-budget] | Least observed ancestor high/max headroom, usage/mapped/reclaim adjustments, then ROOM through existing exec transport. | Not an atomic/exclusive reservation; missing optional readings and concurrent changes cannot be upgraded into a guaranteed balance. |
| [`crates/shards/src/daemon.rs:3243`][shards-spec-call]; [`crates/shards/src/resources.rs:380–405`, `cgroup`][shards-cgroup-values] | Owner run resources become workload settings: positive pids limit → `pids.max`; positive `-m` → `memory.max`, or `memory.high` for existing oom-kill-disable behavior; memory reservation → `memory.low`. | No automatic member grant. This owner's pids-zero/unset conversion must not erase a member's distinct `Some(0)` cap semantics. |
| [`crates/abi/src/run.rs:384–386,663–670`, `Spec::cgroup`, ROOM][shards-abi] | Trusted workload `FILE=VALUE` entries and signed room transport. | No typed guarantee issuer, protected balance or exclusive lease travels here. |
| [`crates/init/src/run.rs:1042–1099,1653–1734,3499–3516`, `cgroups/limit_in/builtin`][shards-init-run] | Guest `/run` with workload, joiners and domains below it; write workload controls; process ROOM through `plug::hold_within`. | Guest limits, current usage and host bounds are different layers, not additive available RAM. |
| [`crates/init/src/plug.rs:185–193,585–631,1117–1127`, `still_pluggable/hold_within/set_high`][shards-plug] | Growth/room control sets run `memory.high` using current use and existing working-room logic. Some existing failed hotplug observations fall back to zero. | Such fallback is not a successful authority/availability receipt. This source observation is not a reproduced defect or an assigned fix. |
| [`docs/architecture/AGENTFILE_ARCH.md:885–889,921–923`][shards-public] | Processes have existing cap/no-spawn/default-VM semantics. `asks.memory` is “a limit it takes on itself, scratch included”, under D59. | An artifact cannot confer a reserved minimum or additional control allowance on itself. |
| [`crates/init/src/domains.rs:468–489,1365–1383`, `domain/start`][shards-domains-start] | Owner-declared process cap takes precedence; otherwise asked process cap. Asked memory is an optional cap. D59 startup pressure is checked `MemAvailable + still_pluggable - workload_remainder`, with saturation at zero for subtraction. | Separate readings are not simultaneous admissions. The variable `promised` does not redefine a cap as a guaranteed owner claim. |
| [`crates/init/src/domains/admission.rs:436–563`, `Parent/Prepared/Committed/workload_headroom`][shards-admission]; [`domains.rs:1611–1622`][shards-member-call] | Hold actual run/domain objects, require controllers, normalize/check caps and keep lifecycle controls before member server/actor launch. Workload remainder is finite max-minus-current; `max` contributes zero to this D59 deduction. | This does not mean an unbounded workload has zero rights. Nor do held descriptors reserve all future management memory, tasks, FDs or kernel work. |

The existing cap admission is the smallest **guest enforcement** seam. The
smallest real **authority producer** seam is earlier: ROOT's owner-request/run
normalization where `Resources` becomes `prepared.spec.cgroup`, before trusted
`Spec` transport and `domains::start`. Any new promise must originate there from
an explicit owner contract, not be synthesized from a member's artifact, a ROOM
observation, parser defaults or the cgroup directory name. This identifies a real
caller pair; it does not prescribe a new public field or introduce an unused API.

## Producer charges and D59 are separate obligations

Current code already has concrete costs beyond an actor-only budget:

- `domains.rs:1564–1584` creates inter-member seqpacket pairs in init before the
  per-member loop; its peer/FD collections are parent-owned. `run.rs:232–243`
  prepares capture/control structures before `domains::start`. An actor's cgroup
  ceiling alone does not account for those retained parent resources.
  [Domain preparation][shards-parent-costs], [capture caller][shards-capture-call]
- `domains.rs:1670–1679` starts the member server before the actor, with the same
  existing member scope/scratch. Server/helper/worker and actor requirements must
  be charged within the actual declared cap, rather than inventing extra tasks
  or treating a requested process count as actor processes alone.
  [Actual member caller][shards-server-call],
  [retained first-runs evidence](measurements/managed-runtime-first-runs/README.md)
- `admission.rs:506–510` opens kill/events controls before member release.
  Acquiring them is real fallible preparation; it is not a reservation for every
  later cancellation/refusal/reap operation. Logical producer charges must remain
  distinct from observed memcg/object charges and from an admitted owner right.
  [Held controls][shards-admission]

**D59 policy cannot be silently replaced by protection settings.**
`Memory::check` reads aggregate domain high/current, then chooses the largest
populated member and calls its lifecycle stop (`domains.rs:998–1034`). This is a
userspace selection, independent of kernel reclaim protection. Configuring
`memory.min` alone would not make that existing choice honor a hypothetical
protected sibling entitlement. This is a source-based boundary for a **new**
contract, not a reproduced current failure or an instruction to alter the policy.
[Actual selection][shards-victim], [owner decision/evidence D59 part nine][shards-d59]

## Explicit owner input still missing

ROOT previously confirmed no established typed guaranteed owner-claim producer;
the traced current inputs retain that boundary. A future strict request needs
all of the following facts from a checked owner policy, not another artifact ask:

| Missing fact | Why this existing boundary needs it |
| --- | --- |
| Meaning and issuer of the right | Reclaim protection, a usable admitted memory/task amount, and an exclusive host backing promise are different contracts. Identify the authenticated owner/run/member and policy lifetime; labels are not grants. |
| A parent able and authorized to honor it | Identify authoritative parent bounds and competing committed obligations, including the primary workload and management. A ceiling-minus-current observation cannot prevent a sibling or another writer consuming it. |
| Actual control and induced-parent charge | Include capture, broker/relay/helper work, retained payloads/FDs and applicable kernel charges before effects; identify ownership through cleanup and uncertainty. No fixed percentage or extra helper allowance follows from these sources. |
| Lending and victim semantics | If borrowing is allowed, specify what is reclaimable and when, and how already admitted rights interact with D59 and owner-requested reductions. Do not infer priorities from size or role. |
| Serialization and lifecycle rules | Establish one authority for admit/commit/release, policy changes and inherited budgets. Held object identity prevents path substitution but does not freeze controller values or reserve capacity. |

No current input can truthfully fill a reserved/available-entitlement value from
this desk research. Those runtime values are **unknown/unmeasured**, not zero or
unbounded. If ROOT introduces a strict guarantee request before supplying its
authority and support, it must return a typed Unsupported/Unadmitted result
before actor release. This note does not claim such a new request/API exists now.

## Unresolved options for ROOT; none selected or implemented

| Option | Existing seam it could use | Required decision/evidence |
| --- | --- | --- |
| Preserve current cap behavior while reporting truthful capability facts | Existing `Resources` → `Spec` → held cap admission. | This is an incremental path, not closure of the hard-resource requirements. Never advertise a new guarantee from its readbacks. |
| Explicit owner-authorized protection policy | Existing hierarchy, with min/low only if ROOT chooses and validates the owner contract. | Define effective ancestors/overcommit/mount semantics and resolve D59 interaction. Protecting used memory is not unused backing admission. |
| Explicit admitted rights and protected control resources | Owner normalization plus the same real pre-release/lifetime seams. | Define a checked producer and parent authority, actual charge model, admission serialization, borrowing/revocation and typed refusal when unsupported. No amount or new topology can be chosen from the present evidence. |

No CPU/IO/storage allocation policy or cross-host topology is proposed. Existing
caps and their valid overcommit semantics remain authoritative. The full managed
plan's hard owner/domain/execution/flow requirements remain open; this research
does not reclassify them as optional.

## Reproduction identities and limits

Primary tags resolve to these immutable source commits (not claims of latest
releases): Linux v6.18 `7d0a66e4bb9081d75c82ec4957c50034cb0ea449`; systemd v258
`781d9d0789379d1ea1f2ecefb804d41e9c8b6c38`; OCI v1.3.0
`92249139eea7161e13745abd4cb6d0ea02a3227a`; cgroups v0.0.4
`a3e2ecd1f756a19cee15f85b96337a59c3b5337b`; runc v1.3.3
`d842d7719497cc3b774fd71620278ac9e17710e0`.

| Retrieved primary file | Bytes | Actual SHA-256 |
| --- | ---: | --- |
| Linux `Documentation/admin-guide/cgroup-v2.rst` | 128488 | `da9a788f2d2ece7b6535c96b56324f28893f8a211768333df7257f586075cd97` |
| Linux `mm/page_counter.c` | 14234 | `71c9bfd3611f44b641d8ea77c385aa3d861124f19a61cfe49b12e412cd8a5246` |
| Linux `mm/memcontrol.c` | 147656 | `537b9bdde4a0adade97279051fbce43590a18390293dd1119af15ea08267cbba` |
| Linux `kernel/cgroup/pids.c` | 11827 | `802a4643e30c5cedd8bd67f5dec91ee28bec802cec9b1169a448ee2f233fd634` |
| systemd `man/systemd.resource-control.xml` | 99883 | `6899aad25cc334478e2f5e13cf1abc137e30a93db9f38974a7317799f347319b` |
| systemd `man/org.freedesktop.systemd1.xml` | 570515 | `2c1c2fe7d97973ecbcd2146f845492449080729223b2a28aeb53bb53388c8cc6` |
| systemd `docs/CGROUP_DELEGATION.md` | 24600 | `7a542a153328ea1af0e7fa15a1ac1e908d9b9484f4d39a804ffc18df18f0a50f` |
| OCI `config-linux.md` | 52544 | `0b9bdad8c8b5ff0ef3e99ff4452c874d9d78a4af347dd145d2b6087e67ca1395` |
| cgroups `fs2/memory.go` | 6453 | `5307e4bf4d14d8d6a50a30f537208d80a2515d04cac1bc5e4872e80056a00699` |
| runc `go.mod` | 1376 | `9248bbff455fb42bdf40bca3057c4077c29baea2336b2eb8b678aba534d4c072` |

Current Shards admission file: 26,462 bytes, SHA-256
`6dd07ec06521349f6c2a11d02db956bb612f768a64c1c24ee45d0cc19758efc2`.
Other exact files are identified by current-main commit/path/line links above;
their hashes can be reproduced without checking out or touching ROOT caches:

```sh
git show a920c85b5c0c9d40aafc4f4cce32432c7c0aed79:crates/init/src/domains/admission.rs | shasum -a 256
git show a920c85b5c0c9d40aafc4f4cce32432c7c0aed79:crates/init/src/domains.rs | rg -n 'fn domain|pub fn start|pub fn check|prepare_child|commit\(d.pids'
python3 scripts/check-development-policy
```

The primary retrieval used Python standard-library `urllib.request.urlopen` at
the pinned raw GitHub paths, hashing returned bytes with `hashlib.sha256`.
No source fetch/interpretation failure is claimed as a product reproduction.
An overly broad filename-filter attempt produced a truncated research-path list;
it supplied no fact or exhaustive coverage claim. The subsequent trace used only
the explicitly cited symbols/files. All runtime capacity, charge attribution,
control-survival and entitlement/mutation results remain unrun by this lane.

## R01–R16 readback and handoff

| Rules | Applicable evidence / remaining boundary |
| --- | --- |
| R01, R03, R04 | Documentation only: no new panic path, literal/policy, state transition or public selector. Future strict refusal and checked arithmetic are requirements, not implemented claims. |
| R02, R05, R12 | Pinned primary interfaces and exact real caller trace, with competing meanings/alternatives. Existing ROOM/placement/wait behavior is described, not audited or replaced; no cache/wait/manager/OS adapter is added. |
| R06, R07, R13 | One bounded note separates facts, inferences and unresolved owner choices. No framework or duplicate implementation. |
| R08, R14 | No performance claim, measurement, thresholds or borrowed benchmark numbers. ROOT owns all profiles, actual parent/kernel/VM attribution and equivalent distributions. |
| R09, R10 | Source observations are explicitly not reproduced defects. Exact revisions, retrieval hashes, call sites and repeatable read commands are supplied; no live before/after/mutation proof is fabricated. |
| R11 | Research concerns the shared Linux guest mechanism; actual host/backend/controller/kernel configurations remain ROOT validation. macOS arm64 only; paused WinARM remains untouched. |
| R15, R16 | Implementer research only; no children/goals/main merge/remote job/local build/cache write or deletion. One named path, signed through gits with required attribution. Frozen source/binaries/evidence preserved. |

Doc-only closing checks: `python3 scripts/check-development-policy` returned 0
with `Development policy checks passed; evidence/coordinator review is still required.`
The named-path `gits diff --cached --check` returned 0 with no diagnostics. No
compilation or runtime test was appropriate or executed for this document change.

ROOT owes the explicit owner contract/producer and P0 decision, then a concrete
scoped implementation assignment through the actual seams, real admission and
cleanup/identity/mutation validation, charge accounting and supported-platform
integration. The mechanical/whitespace checks do not prove the resource contract.
The sustained resource lane remains assigned after this research checkpoint.

[kernel-models]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L684-L740
[kernel-memory]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L1299-L1471
[kernel-controllers]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#controlling-controllers
[kernel-ownership]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L1938-L1949
[page-protection]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/page_counter.c#L294-L464
[page-parser]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/page_counter.c#L264-L289
[memcontrol-max]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/memcontrol.c#L4349-L4449
[memcontrol-kmem]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/memcontrol.c#L2836-L2850
[pids]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/pids.c#L1-L25
[systemd-resource]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/man/systemd.resource-control.xml#L328-L575
[systemd-bus]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/man/org.freedesktop.systemd1.xml#L1581-L1602
[systemd-delegation]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/docs/CGROUP_DELEGATION.md#L55-L74
[oci]: https://github.com/opencontainers/runtime-spec/blob/92249139eea7161e13745abd4cb6d0ea02a3227a/config-linux.md#control-groups
[runc-mod]: https://github.com/opencontainers/runc/blob/d842d7719497cc3b774fd71620278ac9e17710e0/go.mod#L15-L16
[runc-memory]: https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/fs2/memory.go#L35-L77
[shards-resources-input]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/ipc/src/lib.rs#L1130-L1176
[shards-run]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/shards/src/run.rs#L978-L1004
[shards-parents]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/shards/src/daemon/parents.rs#L17-L235
[shards-parent-call]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/shards/src/daemon.rs#L8070-L8115
[shards-budget]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/shards/src/daemon/budget.rs#L25-L178
[shards-spec-call]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/shards/src/daemon.rs#L3243
[shards-cgroup-values]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/shards/src/resources.rs#L380-L405
[shards-abi]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/abi/src/run.rs#L663-L670
[shards-init-run]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/run.rs#L1042-L1099
[shards-plug]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/plug.rs#L585-L631
[shards-public]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/docs/architecture/AGENTFILE_ARCH.md#L885-L923
[shards-domains-start]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/domains.rs#L1365-L1383
[shards-admission]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/domains/admission.rs#L436-L563
[shards-member-call]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/domains.rs#L1611-L1622
[shards-parent-costs]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/domains.rs#L1549-L1584
[shards-capture-call]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/run.rs#L232-L243
[shards-server-call]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/domains.rs#L1670-L1679
[shards-victim]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/crates/init/src/domains.rs#L998-L1034
[shards-d59]: https://github.com/hyper-light/shards/blob/a920c85b5c0c9d40aafc4f4cce32432c7c0aed79/docs/design/architecture.md#L21092-L21186
