# Native domain lifecycle evidence

Policy: `docs/development-rules.md` R01–R16, SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
Implementer: `/root/managed_lifecycle`; baseline source: main `6bf5b15d`.
Root owns live VM runs, audits, mutations, fault campaigns and all measurements.

The fixture installs a native testguest through `AGENT`, with its root `agent.json`.
Its explicitly granted `/probe` volume carries only the test's process facts.
No harness is installed through RUN/VOLUME, and no shell supervises it.
`lifecycle-ready` is published only after the report is synced; the existing
host/main-workload handshake releases the workload after observing that fact.

Run on a supported VM host, from the fixture revision:

```sh
cargo test -p shards --profile test-release --test managed_lifecycle \
  a_supplied_agent_runs_below_a_protected_native_reaper -- --exact --nocapture
```

The expected baseline failure is the recorded actor being namespace PID1.
That is a live fixture hypothesis until the coordinator records its actual run;
source inspection alone is not recorded as a reproduced defect. The fixture also
records proc1 owner and actual signal-zero/proc memory/proc FD-directory access.
It does not claim an exploit from those facts or prove all tamper routes.

The current launch path was compared with Shards' `run.rs` native `--init` reaper,
[runc v1.3.3's fresh init/control handshake](https://raw.githubusercontent.com/opencontainers/runc/v1.3.3/libcontainer/process_linux.go),
[tini v0.19.0's spawn/reap/signal loop](https://raw.githubusercontent.com/krallin/tini/v0.19.0/src/tini.c),
[clone3 CLONE_PIDFD/CLONE_INTO_CGROUP](https://man7.org/linux/man-pages/man2/clone.2.html),
[PID-namespace init semantics](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html),
[wait/reaping](https://man7.org/linux/man-pages/man2/waitid.2.html) and
[cgroup v2 subtree kill/population](https://docs.kernel.org/admin-guide/cgroup-v2.html).
The implementation decision and full every-rule account follow with its code.

This fixture commit changes no launcher behavior. VM red/green, descendant reaping,
helper/actor/owner death, malicious signals/ptrace/proc-FD access, native Windows
and macOS host runs, mutation checks, allocations/faults/latency and seven-target
validation remain owed. Its guard matches the existing supplied-startup fixture;
it is a test-run failure guard, not a production lifecycle timeout.

## Recorded live baseline and light checks

Coordinator real Fedora KVM run r56 at fixture `c9d0a46f`: exit101,
zero passed/one failed in29.50s. Root retained `~/managed-lifecycle-red.log` and
matching environment/policy records. The report was actor_pid1, actor_ppid0,
proc1 owner200000, signal-zero/memory/fd-directory errno0. These are **self-access
facts**, not a parent-tampering exploit. The new green must establish a distinct
actor below a protected UID0 namespace init; root runs that next.

The implementation's exact Rust source hashes and seven native Linux test results
are in `unit.log` (exit0). `clippy.log` is the actual aarch64 Linux-musl init/all-target
warnings-as-errors result. The initial check caught a diverging closure inference
error; a later test build caught a missing TempDir-result unwrap. Both were corrected
before the retained final passing checks. No live VM or mutation/performance check
was run by this implementer.

## Coordinator validation still required

Coordinator KVM green r57 at f756d09d passed1/1 in4.26s, exit0. It verifies the
new actor/reaper identities and denied signal/proc access in the committed test,
not the rest of the attack matrix below. Full raw logs and environment remain on
Fedora as `~/managed-lifecycle-green*`; root retains excerpts in the integration
measurement record.

Root added a one-task scope boundary at eb470a119. It refused startup with
EAGAIN/125 in3.30s (r59). The old-source control c9d0a46f with only that same test
change also refused EAGAIN/125 in2.13s (r60). Thus a one-task scope did not admit
this agent before the reaper: its existing MCP processes already share pids.max.
The original boundary assertion wrongly assumed an actor-only allowance. The
retained boundary checks explicit refusal and that the actor wrote no report;
it does not inflate the owner's task grant to make the new helper fit. The helper
remains counted, and small-limit compatibility/cost beyond this case is still
owed. The old-source test diff is `~/managed-lifecycle-pids-control-fixture.patch`.

- New native-reaper green on actual VMs; inherited-management-FD absence; tamper
  attempts; whole-population cleanup and orphan reaping; helper/owner/actor death.
- Forced setup/exec failure, EOF/invalid readiness, descriptor exhaustion and cgroup
  event/kill failure. The light EISDIR case uses only a private temporary directory:
  a genuine failing pread changes the owner to Faulted, stop closes ownership and
  writes its preheld kill object, and no empty-population success is invented.
- The fatal lifecycle path stops owned scopes, stops the run's other processes while
  preserving independently owned joiners, and reports SYSTEM_ERR/125. Root must test
  convergence and cancellation rather than infer it from this code.
- Existing agents_run_in_their_domains expects `confined pid=1 procs=1`; the new
  topology needs pid2/procs2 and actual reaper-denial proof. Capture's transport
  changes also affect its legacy line assertions; coordinate the combined fixture.
- CLOEXEC launch-error EOF distinguishes **reported** setup/exec errno failures from
  application numeric exit codes. Abrupt death before exec can close that pipe too;
  EOF alone is not proof of an actual exec. Do not claim a guaranteed exec observation
  or use process history as admission. Startup-exec confirmation remains a contract
  limit for root's review and fault campaigns.
- One extra guest process per domain and retained pidfd/cgroup/events/kill/owner FDs
  are real costs. Existing pids/memory hard caps remain authoritative, including the
  helper's cost; no chosen offset inflates them. Measure compatibility of small asks.
- All seven targets and supported KVM/HVF/WHP guest combinations, allocations,
  reallocations/copies/faults, retained/kernel memory/handles, launch/cancel/reap
  distributions and equivalent pinned competitors remain root-owned and unmeasured.

## R01–R16 return account

| Rule | This scoped slice and pending proof |
| --- | --- |
| R01 | New control path uses fallible descriptor acquisition, fixed records/arrays, checked parsing; no production unwrap/expect/assert/panic. Init/all-target arm64-musl clippy passes. Whole-code implicit panic review remains root-owned. |
| R02 | Primary-source mechanisms and runc/tini alternatives above; fresh helper follows the approved plan. Root's live PID1 baseline recorded; real new behavior/cost decision gate remains open. |
| R03 | D59's lifecycle literal table identifies protocol ordinals, field-derived lengths, Linux ABI bits/syscalls/architecture and failure semantics; no tuned timeout/cap introduced. |
| R04 | Prepared launch type; explicit Starting/Running/Stopping/Draining/Ended/Faulted; duplicate completion cannot replace status; kernel population proof never substitutes for reaping. |
| R05 | One new private owned lifecycle module serves domains::start, existing global reap and relay; no speculative public manager/trait. |
| R06 | Focused launch, seal, gate, status, population and reap functions; actual EOF/read-error cases retained; root reviews combined complexity. |
| R07 | Comments identify unsafe-call obligations and non-obvious ABI/ownership sources. |
| R08 | No achieved performance/ratio claim; all 4–5x comparisons owed to root. |
| R09 | Root actual baseline proves direct PID1 identity; new-path EISDIR unit proves the described I/O-error transition only. No invented exploit. |
| R10 | Repeatable native fixture, exact command/hashes/light logs, source and explicit owed actual VM/mutation results retained. |
| R11 | Shared Linux guest code; arm64-musl compiled/linted, native Linux amd64 seven tests pass. Other host/VM combinations remain unverified. Intel Mac unsupported. |
| R12 | Existing clone3/domain/global-reap seams and carried init executable; no host root, extra installed binary, shell supervisor or crypto change. |
| R13 | Small actual-consumer API, no recursive userspace process-authority tree. |
| R14 | No allocation-free/low-cost claim. Root measures additional process/FD/parent/kernel costs and allocation/fault paths. |
| R15 | Implementer ran scoped compile/unit checks only; root owns audit/heavy/adversarial/benchmark/merge work. No subagents or self-merges. |
| R16 | Owned local compiler cache only232MiB when checked; Mac remained above30GiB, last34GiB. No cleanup or user-data/source/evidence deletion. |

## Tenant-local failure update

The initial read-error → whole-run failure rule is superseded by the
[tenant-local fault matrix](fault-scope.md), its typed quarantine/control outcomes
and native9/9 evidence. A domain is a tenant within a potentially shared microVM.
Quarantined retains unknown population and owned references; no claim of full
cleanup/checkpoint readiness is made. Root's mixed-tenant actual-VM/mutation tests
and the separate typed checkpoint-refusal slice remain required.
