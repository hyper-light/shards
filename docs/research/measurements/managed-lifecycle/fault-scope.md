# Tenant-local lifecycle failure scope

POLICY_READ role=implementer task=tenant-local-lifecycle revision=5382f061a609327a2eb491bbbeda9d8093728515
policy_sha256=0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936.
Owned paths: managed.rs, lifecycle-only run.rs hooks, Memory::relieve stop propagation
and the capture owner's approved abort_output compatibility hook. Root owns all
real-VM/fault/mutation/performance/audit campaigns.

A microVM can contain several agents, harnesses or a mixture. A domain is the tenant;
one tenant's failed observation does not by itself invalidate sibling isolation.
The owner's existing hard caps, memory victim policy and explicit grants are unchanged.

## Sources and alternatives

- [Linux6.18 cgroup-v2](https://docs.kernel.org/6.18/admin-guide/cgroup-v2.html), cgroup.events
  and cgroup.kill: observation of populated differs from issuing a subtree kill;
  the kill operation handles concurrent forks and migrations. It does not make a
  failed population observation become an empty-population result.
- [Linux6.18 cgroup source](https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/cgroup/cgroup.c),
  __cgroup_kill, cgroup_kill_write: the held cgroup object selects the subtree;
  an unsupported/dead control returns an error rather than killing unrelated scopes.
- [pidfd_send_signal](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html):
  the held PID descriptor selects a stable process. ESRCH identifies its termination;
  EBADF/EPERM cannot be treated as a successful control operation.
- [containerd Runtimev2](https://raw.githubusercontent.com/containerd/containerd/f00f5261b26b7edb4a4bc870e6539e648a1bc11e/docs/runtime-v2.md)
  explicitly supports one shim servicing multiple containers. Its pinned
  [task service](https://raw.githubusercontent.com/containerd/containerd/f00f5261b26b7edb4a4bc870e6539e648a1bc11e/cmd/containerd-shim-runc-v2/task/service.go)
  keeps Stats failures scoped to the requested container and refuses shutdown while
  containers remain. This is an established scope comparison, not proof of Shards.

| Alternative | Decision |
| --- | --- |
| Stop the whole run for any member-file read error | Too broad when that tenant's held stop authority remains valid. |
| Treat the failed read as empty/Ended | Rejected: invents population proof and could release retained authority/resources. |
| Retry or reopen indefinitely | Rejected: broken observation must not monopolize progress or add unadmitted handles. |
| Quarantine only after held subtree kill plus stable-leader stop | Chosen; stop admission/data, unregister the failed observer, retain unknown state and charges. |
| Continue after containment/identity/status integrity is lost | Rejected; a typed shared-boundary failure escalates through existing run failure handling. |

## Fault matrix and state contract

| Observed failure | Action and result |
| --- | --- |
| Tenant population read error or malformed/missing populated value; stop controls work | Close that tenant's owner channel, issue preheld cgroup.kill, signal its held pidfd; accept ESRCH for an already-reaped leader. Publish one Quarantined update, abort only that tenant's output and unregister its event descriptor. Siblings and main continue. |
| Population-control write fails | PopulationControl{errno}: containment of the server/workers outside the actor PID namespace is unproven. Shared-boundary failure. |
| Stable leader control fails except ESRCH | LeaderControl{errno}: manager-held identity/control integrity is lost. Shared-boundary failure. |
| A completion conflicts with an already-owned status | StatusConflict: shared ownership/reaping invariant failed. No status replacement. |
| Global reap arrives after local quarantine | Add its observed status to Quarantined; never convert the missing population proof to Ended. |
| Owner later disposes of the VM | Existing disposal ends its remaining kernel scope. Until then, owned cgroup/events/kill/pidfd references and their charges remain held, not claimed reclaimed. |

Quarantined is a terminal local service outcome, not a successful empty drain.
`settled` permits disposal of an otherwise finished run; it is not checkpoint-ready.
The separate typed quiesce-refusal slice is required before snapshot/move can claim
success with this state. Recovery/reconciliation is not silently attempted here.
Already committed I/O is not retroactively undone by cancellation.

The capture owner approved a compatibility `Started::abort_output` method for the
old layout: close its out FD and clear its unframed partial. Its capture integration
replaces that body with both typed pipe aborts and tenant credit/control handling.
No sibling/main stream is cleared; already committed output is not rewritten.

## Repeatable light evidence and limits

```sh
cargo test -j4 -p shards-init --profile test-release managed::tests -- --nocapture
cargo clippy -j4 -p shards-init --all-targets -- -D warnings
```

`tenant-unit.log` retains policy/source hashes and 9 passed/0 failed on native Linux.
A private directory produces real pread EISDIR. A native fixture child has a real
CLONE_PIDFD; quarantine stops that child, a separate child's pidfd remains live,
the bad observer is unregistered, and later reap preserves the quarantine's unknown
population. The kill control is a private regular file for this unit case; it does
not prove real cgroup behavior. Additional real EBADF cases classify failed kill
and failed pidfd control. `tenant-clippy.log` is clean. No Mac compilation was run
below the coordinator's disk floor, and no agent ran real-VM fault campaigns.

Root already recorded default reaper green1/1 at f756 and old/new minimum task
allowance both refusing EAGAIN; the 5382 fixture contract is preserved. Root still
owns mixed-tenant real-VM read-error injection, healthy sibling/main continuation,
stop/control integrity failures, guard-removal mutation, data/credit cleanup,
quarantine checkpoint refusal, all supported hosts and all cost measurements.
No real kernel EISDIR defect or escape is alleged by the constructed fixture.

## R01–R16 delta account

| Rule | Evidence / pending proof |
| --- | --- |
| R01 | Fallible held controls and typed errors; native all-target init clippy clean. No new panic/unchecked input allocation. Root implicit-path review remains required. |
| R02 | Kernel and pinned established scope comparison above; root-approved matrix, native real-I/O evidence; heavy live acceptance pending. |
| R03 | No new runtime number, timeout, retry count or resource split. Existing Linux control/errno/ABI values retain D59 sources. |
| R04 | Explicit Quarantined/BoundaryFault/Update; unknown population and observed status remain distinct. |
| R05 | Existing lifecycle, memory stop and relay consumers; named compatibility hook shared with capture owner. |
| R06 | Focused quarantine/control/update paths, no per-error recursive framework. |
| R07 | Comments explain retained-proof/FD/ABI obligations. |
| R08 | No performance ratio claim; equivalent comparisons remain root-owned. |
| R09 | Constructed native EISDIR/EBADF effects only; no old exploit inferred. |
| R10 | Exact source hashes, commands and light logs retained; root live/mutation results explicitly owed. |
| R11 | Native Linux amd64 exercised; other seven-target/real-VM combinations unverified for this delta. Intel Mac unsupported. |
| R12 | Same Linux guest implementation on all hypervisors; no host-root, binary, shell or crypto addition. |
| R13 | One local transition plus existing shared-failure route, no second manager. |
| R14 | No zero-cost/reclamation claim; retained handles/charges documented for root profiling. |
| R15 | Agent light tests only; root audits, profiles, runs heavy/fault/chaos/mutation campaigns and merges. |
| R16 | No local compilation after root's30GiB floor warning; no cache/data/source/evidence deletion. |
