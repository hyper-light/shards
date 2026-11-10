# Selective main-workload cleanup: scoped real-VM proof

Current native readiness controls require the caller-supplied absolute Rust
artifact `CLEANUP_CLIENT_CHILD`; see [readiness.md](readiness.md#current-shared-native-child-portability-checkpoint)
for pinned compile and Python-free Windows commands. The new artifact is private
test input, never installed/carried/product output. This document retains the
historical real-VM evidence below; it is not a new revision passing gate.

The proposed cleanup/progress defect was **not reproduced**. All four bounded
real-VM cases reaped every owned child and preserved a fresh independent-joiner
operation. Production cleanup is unchanged. The picked 64-pass value remains
unjustified; this checkpoint neither certifies it nor substitutes another number.

## Policy, source and ownership

`POLICY_READ`: canonical `c921a0b12b06d495b354907162e991376edffd41`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`, role implementer,
`/root/constants_research`; coordinator `/root`. All R01–R16, AGENTS, evidence
checklist, CLAUDE, managed-runtime plan and current handoff reread. Scoped live
ordinary-child testing was assigned under R15. No takeover/ptrace/FD-theft campaign,
production remedy, subagent or main merge is part of this batch.

New branch/worktree `p/managed-cleanup` / `managed-cleanup`, based on combined
`b8febd1c3b988453d12c0690c27f7e53a2ac24a0`. Owned source is only the new
[host fixture](../../../../crates/shards/tests/managed_cleanup.rs),
[guest fixture](../../../../crates/testguest/src/cleanup.rs), one cfg-Linux module
declaration and exactly two testguest workload dispatch arms. Existing managed
reaper modules/dispatch arms are preserved. Init `run.rs`, `managed.rs`, `join.rs`,
root capture and all other implementer paths stayed read-only. Rego proof593 and
experiment ca76 remain clean and separate; no Rego production adoption occurred.

Private Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, GNU/Linux amd64 KVM,
host kernel `6.19.10-300.fc44.x86_64`, Rust `1.98.1`. Guest is the actual pinned
`vmlinux-6.18.48-x86_64-17c3ba205400`, not a mocked process namespace. The kernel,
init, guest, CLI and helper identities are retained with the evidence.

The unchanged admitted envelope was **4 CPUs / 8 GiB RAM / 20 GiB disk**, cargo
`-j4` and inherited `CARGO_BUILD_JOBS=4`. Private synced clone:
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-cleanup-a4aff8513b`; private
`~/managed-cleanup-target`; raw evidence `~/managed-cleanup-evidence`. The target
was reflink-copied from previously approved inactive `~/managed-output-endpoint-target`
without writing that source, then workspace package artifacts were cleaned in the
private target. Root's active `~/managed-capture-check` and target were not used.
The testguest nested target is inside this private clone. No Mac build ran.

After r57's live cases completed, the coordinator relayed the newest bootstrap
role boundary: profiling, benchmarks, audits, heavy real E2E, chaos and sweeps
belong to the coordinator; agents do scoped implementation/light tests. No further
campaign/audit/production change was started. The already-running focused clippy
checks finished, followed only by evidence collection and this checkpoint.

## Actual callers and authority

At b8febd1c, `init::run::kill_all_but_joiners` is called:

- In the main process's SIGCHLD/reap branch when independent joiners remain; it
  spares joiner cgroups and explicit helper children working for them.
- When host-network relays or joiner layer senders must finish after main exits;
  their held PIDs are passed as the explicit spared set.
- After a shared lifecycle-boundary failure, after attempting owned domain stops;
  independent joiners keep their existing ownership.

The no-survivor case already calls `kill(-1, SIGKILL)` rather than this sweep.
`Born::Own` (the default) makes main PID1 of a private namespace; `Born::Reaped`
(`--init`) first creates the legacy protected namespace reaper; `Born::Host`
(`--pid host`) runs in the VM init's namespace. Ordinary main processes use
`/sys/fs/cgroup/run/workload`; independent joiners use `/run/join-ID` siblings.
The newer managed `Launch` holds cgroup.kill/events and a pidfd for each declared
AGENT/HARNESS domain. It does not own every ordinary main process and helper.

Therefore managed-domain cgroup cleanup is not a demonstrated replacement for
this caller. Killing the aggregate `/run` cgroup would include independently owned
joiners. Killing only `workload` would not itself settle every legacy domain/helper
and spared-layer/relay obligation. The **need for selective lifetime cleanup** is
observable in the `--pid host` cases: controlled children do not self-exit on main's
control connection closing, yet they are reaped while the VM/joiner continues.
This does not establish that userspace `/proc` sweeps are the required mechanism.

The 64 bound originated with commit `1925841de80037e220c772ff8ad3e4cc0f2aba0a`
(D119 network joiners), including the comment that a process forks at most once
between looks. A second 64 loop was added to `join::kill_rest` by
`a4b31f32a4d1265786e70877eb1047f36635a2ed` for shared PID namespaces. The inspected
commit records/D119 provide compatibility intent and old suite results, but no
numeric derivation or concurrency proof for 64. It is classified **unproven**, not
protocol/ABI bound, learned value or established kernel invariant. The second loop
was inventoried read-only, not exercised as this batch's main target.

## Primary kernel and established alternatives

Exact fetched source bytes, SHA-256 and tag commit identities are in
[sources.json](sources.json). These are semantic/source comparisons, not live
cross-runtime benchmarks:

- [Linux v6.18 cgroup v2 contract](https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/admin-guide/cgroup-v2.rst),
  `cgroup.kill`, defines subtree SIGKILL handling concurrent forks/migration.
  `cgroup.events` population changes provide completion notifications; kill issuance
  and proof of population exit are separate facts. No selected userspace pass
  count follows from that contract.
- [Kernel cgroup implementation](https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/cgroup/cgroup.c),
  `cgroup_kill_write`, `cgroup_kill`, `__cgroup_kill`, selects the held subtree under
  kernel synchronization, tracks a kill sequence, skips kernel/already-dying tasks,
  and signals descendants. It does not turn an unrelated root/helper PID into a
  member of the selected owner.
- [Kernel PID namespace teardown](https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/pid_namespace.c),
  `zap_pid_ns_processes`, disables new PID allocation, signals remaining namespace
  tasks, then waits until children are gone. This explains why a private PID1's
  death has a different cleanup contract from `--pid host`.
- [runc v1.3.0 container signaling](https://raw.githubusercontent.com/opencontainers/runc/v1.3.0/libcontainer/container_linux.go)
  distinguishes private from shared PID namespaces. Its
  [signalAllProcesses](https://raw.githubusercontent.com/opencontainers/runc/v1.3.0/libcontainer/init_linux.go)
  uses cgroup.kill for SIGKILL when available; the fallback freezes, enumerates,
  signals and thaws. This demonstrates an existing owned-cgroup seam, not a reason
  to copy a fixed pass count or silently fall back after an arbitrary refusal.
- [Tini v0.19.0](https://raw.githubusercontent.com/krallin/tini/v0.19.0/src/tini.c)
  waits for signals, forwards them, drains waitpid(WNOHANG), and exits when main's
  status is known. Its optional subreaper role handles adoption. This reaping role
  is distinct from selecting and ending an entire shared-namespace cgroup.

Linux v6.18 is the requested primary reference; the actual guest is Shards' pinned
6.18.48 build with its patches. This source review is not a claim to have built or
byte-compared the running kernel against upstream v6.18.

## Live fixture and actual outcomes

The guest starts a bounded number of ordinary children through the real command
path. Each child acknowledges its PID over a local Unix socket before main says
ready. Children close output and then pause independently of the socket; main's
exit cannot naturally release them. No child churns or grows an unbounded tree.
The release exec is acknowledged and reaped before main returns, avoiding a
fixture-created race where cleanup kills the command that delivered release.

A separate retained joiner uses `--pid host`. After readiness it takes **one**
`/proc` discovery snapshot matching only the fixture's exact child mode and unique
synthetic token, opens those known-live children's pidfds, then waits for kernel
`POLLHUP` (reaped), distinguishing it from exit-only `POLLIN`. There is no periodic
`/proc` loop. It never signals a discovered PID or addresses a foreign process.
Its wait has a fixture failure deadline; no such deadline enters production.

Kiln r51: initial real-VM campaign **1 passed, 0 failed, 1 ignored helper**.
Kiln r57: final explicitly selected ignored campaign **1 passed, 0 failed**, with
only the unrelated common port helper filtered out. The explicit campaign is
ignored in ordinary suites because it requires an actual hypervisor; when invoked,
it asserts a real backend rather than treating unavailable hardware as a pass.

| Case | Actual owned children | Reaped through pidfd | Remaining | Fresh independent joiner write/read |
| --- | ---: | ---: | ---: | --- |
| Default private PID namespace | 8 | 8 | 0 | Passed |
| `--init` | 8 | 8 | 0 | Passed |
| `--pid host` | 8 | 8 | 0 | Passed |
| `--pid host` | 72 | 72 | 0 | Passed |

72 is a synthetic count beyond the source's unrelated 64-pass literal; number of
processes is not number of enumeration passes. The fixture does **not** prove that
64 passes were reached, sufficient for concurrent forking, or necessary. None of
these children forks after its ready acknowledgment. No scheduler-controlled
fork/enumeration race, permission/read failure, stuck kernel task, population
migration or unusual helper-spared combination was injected.

The final known-live control deliberately withholds main's release for 100 ms.
A second observer returns timeout and prints exactly eight still-sleeping owned
children. This is the expected fixture control, not a product cleanup failure.
The original observer remains active, then observes all eight reaped after release.
There is no production fix or guard removal in this read-only batch, and no
production mutation result is claimed.

Single final observations of observer lifetime are 114.913 / 7.398 / 9.567 / 11.737
ms in table order; the first includes the deliberately held-live control. These
include release/CLI/scheduling time and are not isolated sweep latency or meaningful
performance distributions. No 4–5× competitive result, allocation/copy/kernel-cost
or leak-free claim is made. Reaping of the known children and surviving joiner
progress are the specific measured cleanup facts.

Final focused validation: strict native `cargo clippy --locked -j4 -p shards
--test managed_cleanup -- -D warnings` and `cargo clippy --locked -j4 -p
shards-testguest --bin shards-testguest -- -D warnings` both exit 0. Policy checker,
owned-file rustfmt, diff whitespace and shell syntax pass. These are scoped checks,
not a complete workspace/platform result.

## Decision and remaining work

No cleanup defect was reproduced, so **no production source remedy is proposed**
from these data. Keep the existing code held for coordinator review. The 64 literal
and its comment still lack adequate R03 authority; an old suite passing does not
supply that authority. Replacing 64 with another cap, a periodic poll, cache or
parallel manager is not justified.

If the coordinator assigns implementation next, first map every actual caller's
owned cgroups and spared helpers. Compare the existing held-cgroup kill/completion
seam with namespace teardown for private PID1, preserving independent joiner and
layer/relay completion contracts. A cgroup design needs a real owner for each
selected process and explicit kill-issued → population-empty → reaped/terminal
states, including typed refusal/uncertainty when the proof is unavailable. That
requires separate reviewed source scope and live evidence before adoption.

Still unrun: concurrent fork/enumeration schedules, the separate joiner kill_rest
loop, nested cgroups, failure/refusal/retry, main/shared-boundary failure variants,
all layer/host-relay exceptions, actual memory/FD/kernel charge return, allocation
and CPU/fault/copy distributions, interference and competitor comparisons. Other
six host targets/libcs/distributions and HVF/WHP/hardware combinations remain
unverified. Paused Windows ARM VM was untouched. Previously provider-rejected
security cases remain unverified and outside this ordinary lifecycle task.

## R01–R16 accounting

| Rule | Evidence / remaining limit |
| --- | --- |
| R01 | Only test binaries/fixtures changed; init bytes unchanged. Guest fixture propagates I/O/refusal errors; no product OOM/panic safety certification. Test-only allocations remain ordinary library allocations. |
| R02 | Pinned primary kernel/runc/Tini sources, caller/history review and actual KVM cases; no architectural adoption. |
| R03 | 64 classified unproven, no substitute added. Child/sample/deadline values are fixture inputs only, never product policy. |
| R04 | Ready → release-controller-reaped → main-ended; known child running → exited/POLLIN → reaped/POLLHUP. Actual counts and live negative control recorded. No new product outcome protocol. |
| R05 | Existing lifetime/cgroup/namespace seams reviewed; no manager/cache/poll added. Necessary selective cleanup is distinct from justification for the particular sweep. |
| R06 | Focused guest/host fixture; one discovery snapshot then event wait, bounded synthetic population. |
| R07 | Comments explain independent child lifetime, token selection, reaping distinction and control-release race. |
| R08 | No equivalent competing runtime performance campaign; no achieved 4–5× target or isolated speed claim. |
| R09 | Ordinary cases actually run before any production edit; hypothesized defect not reproduced and therefore not “fixed.” |
| R10 | Repeatable script, exact source hashes, compiler/run logs, observed counts, negative-control result and binary identities retained. No invented production mutation pass. |
| R11 | Native GNU/Linux amd64 KVM only, actual guest musl binary; other six host/hardware/libc combinations unverified. |
| R12 | Real shared Shards lifecycle path, no installed binary, host root, grant relaxation or crypto weakening. Testguest is an existing fixture binary. |
| R13 | Small owned fixtures; production source untouched, larger ownership migration remains separate review work. |
| R14 | Exact known-child reap/progress counts; observations not distributions. Allocation/copy/PSS/kernel/neighbor cost remains unmeasured. |
| R15 | Assigned lane/envelope/readback, named-path signed checkpoint, no children/main merges/interruptions. Coordinator owns final combined gates and any future remedy scope. |
| R16 | Private clone/target and inactive reflink source preserved; only own regenerable artifacts cleaned. No guard bypass or Mac compiler/test. Source, evidence and old checkpoints retained. |

## Reproduction and retained artifacts

From an isolated checkout on an admitted real KVM host:

```sh
export CARGO_TARGET_DIR="$HOME/managed-cleanup-target"
bash docs/research/measurements/managed-cleanup/run.sh "$HOME/managed-cleanup-replay"
```

Initial source is b8febd1c plus the fixture snapshot named in `source.sha256`.
The final checkpoint contains the final tested fixture bytes; r51's earlier fixture
is exploratory evidence. Source freshness is explicit: private workspace clean for
r51; owned testguest/test sources touched before the final rebuild. Both logs show
the compiler rebuilding actual fixture sources; no restored-byte/mtime assumption
substitutes for a rebuild. The final init/kernel/CLI bytes are hashed separately.

[report.json](report.json) records actual outcomes and final binary identities.
[results.tar.gz](results.tar.gz) preserves 18 raw compiler/run/status/source records,
verified by [results-manifest.json](results-manifest.json). The artifact catalog
includes the carried helpers directory's empty GPU placeholder; it was not executed
or tested. r51's exploratory guest executable was not frozen before the final
rebuild; its source/logs are retained, while final binary identities cover r57.
[validation.log](validation.log) records local static and archive/source verification.

The artifact manifest and archived raw logs accompany this note. No binaries are
embedded in the repository; the final frozen copies stay in the private evidence
folder. No source/runtime cleanup implementation was changed to produce green.


## Owed coordinator command and role handoff

No lane job remains active. This is a runnable signed fixture/research checkpoint,
not a production cleanup decision. The coordinator owns any repeat real-VM
campaign, caller/ownership audit, benchmark, mutation and exact combined gates.
On the reviewed combined source in a coordinator-owned private checkout, retain
the exact **4 CPU / 8 GiB RAM / 20 GiB disk** admission request and run:

```sh
export CARGO_TARGET_DIR="$HOME/managed-cleanup-coordinator-target"
export CARGO_BUILD_JOBS=4
bash docs/research/measurements/managed-cleanup/run.sh "$HOME/managed-cleanup-coordinator-reviewed"
```

That reruns only the committed bounded ordinary-child fixture. The broader unrun
cases listed above need separately scoped coordinator commands; no unimplemented
race fixture or production mutation is claimed runnable. A future remedy must
first have its own concrete failing case and reviewed narrow production scope.
