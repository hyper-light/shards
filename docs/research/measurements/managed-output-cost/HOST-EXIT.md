# Owned daemon/VM exit readiness — test source only

SOURCE `8c8625b614f543e1b490faee19a4cc4917166286`, fresh
`p/managed-output-exit` from combined
`95e3b55e420199cd29a5649ad690d554dbf73942`. Historical cost branches/artifacts
remain unchanged. **No live proc, handle, FD, resource, VM, profile, audit or
mutation execution was performed by this implementer.** Actual caller type/lint
and every live proof are coordinator-owed. Formatter/policy/source checks are
the limited agent evidence, not an exit result.

Root's previous r497 at f062 compiled the real callers and ran managed_output
6pass/2ignore plus actual KVM member counts0/1/2 successfully. The fixture then
explicitly left host exit/kernel return unverified. Root's later read of only
those six own daemon/VM records found stat ENOENT; that is later absence, not
held-handle exit timing or kernel/population proof. Root retained r497 and r500
guard-removal/restoration evidence in `425b57ad`. None becomes a passing test of
this changed source or a newly observed defect. This extension adds the missing
coverage contract; it does not fix a proven production lifecycle defect.

## Existing facility and alternatives

Reuse `shards_vmm::platform::{Poller,ExitWatch}` at the exact base:
`platform/unix.rs`1873–1985/2043. `managed_cleanup.rs` already uses this facility.
On Linux the actual existing implementation owns an epoll FD, and each
`add_exit(pid,token)` owns one unique CLOEXEC pidfd opened with flags0. No new
platform adapter, API, dependency, syscall wrapper, generic manager, callback
interface, cache or shared shutdown change is introduced.

Alternatives considered narrowly: an opened proc directory preserves task
identity but is not pollable; a new pidfd wrapper duplicates the established
facility; `waitid(P_PIDFD)` is for a child and would not prove reaping of these
non-child daemon/VM processes. The fixture never calls waitid, signals a PID,
discovers a replacement target or retries a numeric target after identity failure.

Primary contracts:

* [pidfd_open(2)](https://man7.org/linux/man-pages/man2/pidfd_open.2.html): flags0
  names a process rather than a thread; readability follows the last thread's
  exit. Readability and hangup have different zombie/reaping implications.
* [Linux v6.18 pidfd_poll](https://github.com/torvalds/linux/blob/v6.18/fs/pidfs.c):
  readiness waits for group-leader exit constraints; an absent task reports
  readable plus hangup. This source is pinned; actual host-kernel validation is
  still owed.
* [wait(2)](https://man7.org/linux/man-pages/man2/waitid.2.html): child-state and
  reaping semantics do not make an arbitrary non-child waitable.
* [epoll(7)](https://man7.org/linux/man-pages/man7/epoll.7.html): these unique,
  nonduplicated watch descriptors leave epoll when closed. A ready watch must be
  retired to avoid repeatedly receiving its level-triggered readiness.
* [kernel proc documentation](https://docs.kernel.org/filesystems/proc.html):
  an opened proc directory's operations do not act on a new task reusing the PID;
  they may fail after the original task exits. Holding it does not preserve life.

The existing Poller erases event flags, so this fixture reports only
`ExitReadyObservedAfterOwnerStopWithinCleanupDeadline`. It does **not** report
reaped, exit status, exact exit time, stop causality, full population return or
kernel resource return. Those remain explicitly unverified.

## Ownership and readiness

The proof holder lives outside the existing protocol `catch_unwind`: absence
means acquisition was never reached and is a failing NotPrepared outcome. Partial
acquisition lives in the holder as explicit Unavailable roles. Expected identity
comes only from this fixture's daemon record and actual inspected VM host PID.

For each fixed role, acquisition opens its proc directory, requires the expected
start, parses stat with the **existing** `parse::process`, checks PID/start before
`Poller::add_exit`, then checks again through that same pinned directory. An I/O,
parse, start or registration failure drops partial objects and records the typed
stage/cause plus known expected identity. There is no authoritative inference
from later ENOENT and no repeated unheld `/proc/PID` identity lookup.

Acquisition occurs at initial run/identity readiness, before any observer starts;
the handles remain owned through protocol readiness and owner shutdown. At the
existing published readiness checkpoint the same pinned identities are checked
again. Per-role readiness is Pending → Matched or Unverified. Pending/Unverified
cannot become a passing exit proof merely because an event later occurs.

After the unchanged owned shutdown/observer join, one immutable caller cleanup
deadline governs event waits. Each call uses remaining time; an empty/EINTR
return re-enters that event wait, not a process-state probe or periodic poll.
Each matched event consumes its watch and directory immediately, preventing a
ready role from spinning while waiting for the other. An observed event after
the deadline is conservatively Deadline. Unavailable/Deadline remain failing;
only **both valid roles** reaching ExitReady may pass the exit proof.

## Exact FD accounting without a shutdown exception

1. Preserve the original observer baseline and sample B before watch acquisition;
   require their exact equality, so an intervening existing helper leak cannot
   be hidden by rebasing.
2. Count actual retained objects: a successful existing Linux Poller owns1 FD;
   each actual Watching variant owns1 proc-directory File plus1 ExitWatch pidfd.
   A partial failure that drops its objects contributes none. Require measured
   with-watch count exactly B plus these objects; there is no slack allowance.
3. The existing observer-return equality remains unchanged, using the measured
   with-watch baseline while all observation objects remain retained.
4. After the event wait or any error/timeout, drop all remaining watch/directory
   objects and the poller, then independently sample and assert total exact B.

The self-FD enumeration opens its own temporary directory FD consistently in
each sample; this is observation overhead, not an uninstrumented count. Records
separate original/B/with-watch/observer-return/after-release phases. File reads,
poller storage, FD enumeration, serialization and all observers have parent
costs; none is subtracted or advertised as free. Samples are non-atomic.

Owner stop is attempted even after a protocol panic. Handles are released and the
total FD check is attempted before rethrowing the original failure. The teardown/exit/FD record is written after every handle is dropped, then direct
FD-return and strict exit assertions run before the original protocol panic is
resumed. There is no second unwind state. The original observer/terminal checks remain and are not converted
to a generic success. Source-only cleanup reasoning is not an executed proof.

## Source-review simplification, no runtime bug claim

The earlier signed `07dd26ae4eb840688e9419c18fff3ef9a880947a` checkpoint remains
unchanged in Git and was never adopted or executed by this agent. Root's source
review requested concrete simplifications, implemented in the follow-up:

* Existing `ObservationFailure` now carries the actual additional producers;
  redundant `ExitCause`/`ExitFailure` layers are removed.
* Named `observed_exit`, `deadline` and `wait_failed` transitions replace the
  boolean `retire(event, expired)` interface. Pending/Unverified readiness still
  cannot be promoted to exit success.
* One `ExitFds` value owns the validated B, measured with-watch count and actual
  held-object count. It is rendered once in teardown with the after-release
  sample; no parallel FD/error records are maintained at readiness.
* The second `catch_unwind` is removed: release → sample → write teardown → direct
  FD/exit assertions → resume any protocol panic. The original protocol unwind
  boundary still owns shutdown and partial-handle cleanup.

These are source/design corrections to an unadopted test helper, not live fixes,
performance findings or a passing runtime gate. All original shared shutdown,
observer, terminal, scope, byte and resource-return checks remain unchanged.

## Local evidence and owed coordinator work

[records/host-exit-source-checks.json](records/host-exit-source-checks.json)
records formatter, policy, whitespace and exact dependency/source checks. The
existing parser and every shared helper/platform file are byte-identical to the
base. No extracted fake caller, invented guest artifact, new unit framework or
runtime success claim is used to replace the actual integration compile.

Coordinator owed: actual normal-helper caller compile and strict supported
matrix; real N0/N1/N2; both exit tokens before the same cleanup deadline; partial
open/register/identity failures; protocol error and timeout cleanup; exact FD
return with each observation object retained/released; guard-removal/restoration,
wrong/stale identities and other adversarial cases; all before/after costs and
parent/kernel accounting. Existing Poller's internal fixed event batch and Vec
allocation remain unchanged dependencies outside this scope and need coordinator
review. No broad panic-free/OOM/leak-free claim follows.

POLICY_READ implementer: full canonical g121-main AGENTS/R01–R16/evidence,
CLAUDE/managed plan/current handoff; main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
R01/R04/R06/R07/R13: test-only explicit resource/proof states, strict failures,
unchanged production and shared checks. R02/R03/R05/R12: primary contracts and
established facility reused, actual object counts and role tokens only, no new
runtime constants/cache/polling/manager. R09/R10: prior root facts retained as
their narrower facts; changed live proof is owed. R08/R14: no performance or
allocation result, root owns all costs. R11: Linux fixture only, matrix unrun.
R15/R16: scoped signed source/evidence only, no children/main merge/root cache
write/local build/cleanup or paused Windows ARM action. Historical Mac OOM,
UDP coverage, hard budget and management-plan gaps remain unchanged.
