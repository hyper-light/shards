# Reuse owned containment for capture failures

Implementer follow-up on df47bc0658d47025a393df8f951391c8b0a68801; the resumed
POLICY_READ receipt at68391c398 has the same current policy SHA256
0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936.
All R01–R16 and root-only heavy/audit/profile responsibilities remain binding.

Capture has an actual consumer for tenant-local allocation/resource failure.
It must reuse held-scope containment rather than duplicate cgroup/pidfd killing
or end the whole shared VM. The existing [kernel/source/established scope
comparison](fault-scope.md#sources-and-alternatives) and root-approved matrix
still apply; this adds no new process manager or isolation mechanism.

`Owned::quarantine(QuarantineCause)` is now the one shared path.
`QuarantineCause::{PopulationObservation{errno},Capture{errno}}` preserves the
first actual source and errno in State and Update. It cannot mislabel a capture
failure as a failed population read. The existing relay applies that typed update,
passes cause.errno() to abort_output, and displays the actual cause. No caller
parses diagnostic text. No new production numeric tuning literal is introduced.

| Current state | Request outcome |
| --- | --- |
| Starting/Running/Stopping/Draining | Close owner admission, use the preheld subtree kill and stable pidfd; successful containment gives one Quarantined{cause} update. Control failures retain their typed BoundaryFault. |
| Quarantined | Observed; first cause/status/unknown population remain, no repeated stop control. |
| Ended | Observed; proven emptiness/status are preserved. The capture caller independently aborts its failed data/control stream. |
| Faulted | Return the same prior BoundaryFault; never downgrade lost shared control into a local quarantine. |

Quarantine still retains bounded owned references/charges and unknown population;
no reconciliation or reclamation is invented. A later observed reap adds its status
without replacing the cause. The quiesce refusal retains the cause's actual errno
while reporting the typed Quarantined eligibility reason.

## Light evidence and composition requirement

`quarantine-cause.log` records exact source hashes/environment and12/12 native Linux
lifecycle unit passes. A real CLONE_PIDFD fixture child is stopped through the shared
API with a Capture{ENOMEM} cause. This is an explicitly supplied typed cause, not a
claim of a live allocator failure. The private kill object is a regular fixture file,
not a real cgroup; root owns real capture failure, cgroup and mixed-tenant campaigns.
The second request cannot replace the original cause; later reap keeps it. Another
case uses invalid stop descriptors to prove terminal Ended/Faulted requests issue no
new stop operation and preserve their proof/error. Existing real EISDIR/EBADF cases
continue to pass.

The production all-target clippy step reports Capture as dead_code in this standalone
branch: the only production capture caller is in the concurrent capture lane. No
allow/expect waiver, dummy use or duplicate capture implementation was introduced.
Root approved this explicit composition dependency. The actual caller constructs
QuarantineCause::Capture; the coordinator must rerun combined init all-target clippy
before calling the batch ready. The separate quiesce checkpoint's checks remain
recorded at their exact revisions.

The host quiesce units now assert typed Busy/Refused/Ended variants; the single
presentation assertion stays separate. `quiesce-typed-assertions.log` records3/3
Mac arm64 passes under lockf/-j4, above30GiB. No new real-VM, chaos, mutation,
benchmark, allocation/fault profile or seven-target sweep ran in this lane.

The [quiesce record](quiesce-refusal.md) additionally names existing per-run daemon
save serialization and the unverified Hold/direct-protocol concurrency boundary.
No concurrency fix was inferred or implemented without a live reproduction.

## R01–R16 delta account

- R01/R04: fixed typed cause and explicit terminal transitions; no new allocation,
  panic, unchecked length or unrelated state reset. Combined production lint is
  pending the real capture caller, not represented as passed.
- R02/R03/R05/R12: reuse the source-backed existing held-control path and actual
  capture consumer; no new ABI number, resource entitlement, platform workaround,
  shell, installed binary, host root or crypto change.
- R06/R07/R13: one focused entrypoint and cause enum, existing update hook, safety
  comments retained; root reviews the composed complexity.
- R08/R14: no achieved performance or zero-overhead claim. Root owns all equivalent
  comparisons and allocation/copy/fault/parent/kernel cost evidence.
- R09/R10: native typed-cause/terminal cases and real EISDIR/EBADF unit evidence only;
  actual capture OOM/failure, root mixed-VM fault and mutation results remain owed.
- R11: native Linux amd64 light evidence and Mac arm64 host test-only assertions;
  all other supported seven-target/VM combinations unverified for this delta.
  Intel Mac remains unsupported.
- R15: coordinator approved exact lane/consumer dependency; no children, self-merges
  or heavy runs. Return signed source/evidence and stop; root composes and validates.
- R16: headroom guard and Mac floor honored; no cleanup/deletion of source, user
  data or evidence. Local/remote owned build caches are reported for coordination.

Latest event-driven steering: this follow-up adds no timed sampling, retry loop,
cache or duplicate reconciled copy. It reuses held pidfd/cgroup control and existing
event readiness. First cause is authoritative lifecycle outcome, not a performance
cache. Existing mechanisms were not removed without live evidence.
