# Bounded owned-child crash fixture proposal

Status: coordinator approved the bounded implementation and caller-supplied task
extension, then approved the private one-byte stdin ordering amendment on2026-11-10.
No helper, socket, VM, fault,
stress/chaos campaign, audit or profile has been executed. Preserve normal-exit
checkpoint `fce89fb1063b5c2344f920ff9e967c023ce3abfc` unchanged on
`p/managed-tenant-exit`. This proposal uses the same worktree on
`p/managed-tenant-crash`, based exactly on that checkpoint.

POLICY_READ implementer: canonical main
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`, rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Full AGENTS/R01–R16/evidence/CLAUDE/managed-runtime plan/current g121-handoff read
again for this batch. Root owns actual campaigns, all audits/profiles, final
review and main merges. No children; no new full worktree/local build; paused
Windows ARM and named root sources/targets remain untouched.

## Command and ownership contract

Proposed native forms:

```text
managed-capture TOKEN crashes COUNT WORK_MS CLEANUP_MS REPORT_BYTES
managed-capture TOKEN crash-child ROUND
```

The parent checks positive finite count, deadlines, representable absolute
`work_until` and `cleanup_until = work_until + cleanup_ms`, and checked total
report-byte requirement against the supplied envelope before spawning. There is
at most one owned unreaped `std::process::Child`. The parent fresh-execs the same
native testguest with literal arguments, one private piped stdin and inherited
fd1/fd2. The child receives only the read end as fd0; the parent owns the writer
and closes it promptly after its one-byte release. No parent management handle
is passed as this handoff. No
shell, raw fork, pre_exec hook, new process manager, credential change or grant/
syscall/profile widening. Original descendant confinement remains in place.

Each round is `Spawned -> HeldPidfd -> Released -> ExitReady -> Reaped -> next round`.
The fresh child blocks on the single release byte BEFORE core suppression and
the intentional self-signal. Only successful held pidfd acquisition permits the
parent's release write, and the original work deadline is checked before that
write. Acquisition/write/EOF failure stays typed and retains Child cleanup. The
cleanup path closes any unreleased stdin writer before killing/waiting only its
owned child. EOF from parent exit also refuses the child before its fault. The
pipe wait is a one-shot readiness dependency, not a periodic probe or new grant.
Use one original absolute work deadline with the existing native readiness wait;
no try_wait/sleep probe. The parent calls actual Child.wait after readiness and
records raw wait status, signal, optional normal exit code and core-dump flag.
The parent emits an independent progress record after each reaped child and an
exact final binary stdout/stderr pair after all rounds, then completes normally.
No next round begins until the previous child was actually waited on.

All fallible work after spawn stays inside a local closure that leaves Child
owned by the outer error path. Failure kills only that Child and attempts finite
pidfd readiness plus actual wait under the fixed cleanup deadline. Cleanup
expiry records unreaped/unknown and admits no next child; it does not claim that
dropping Child reaped it. The outer run owner remains responsible for population
cleanup, and root supplies its independent final proof. A blocking spawn/kernel
operation is not advertised as a universally hard wall-clock bound.

Resource envelope awaiting root choice: count/report bytes/deadlines/max1 child
bound fixture work, but do not establish aggregate/parent/kernel accounting.
`Fixture::start_with_options` currently hardcodes `AGENT helper`; AGENT's default
Processes::Unbounded inherits the VM limit. Explicit `AGENT --processes=TASKS`
would require a tiny caller-input extension in the now root-reserved host parent
function. Root must approve that patch or provide its outer VM task/memory/CPU
envelope. No invented task reservation or passing hard-budget claim.

## Core suppression and alternatives

The existing `init::capture::tests::allocation_refusal_keeps_its_resource_cause`
uses own-process RLIMIT_CORE soft/hard0. The proposed fresh crash child repeats
that narrow action, verifies its own effective limit, sets and verifies its own
dumpability0 after exec, resets/unblocks its own SIGSEGV, emits controlled bytes
and raises SIGSEGV only on itself. Any suppression failure returns a test failure
before the intentional signal. No host sysctl, host settings or product security
code changes.

RLIMIT_CORE alone does not prevent piped dump handlers; that exception is why the
proposal also disables the child's dumpability. Linux6.18 coredump_skip checks
non-dumpability before dispatching the file/pipe/socket dump handling. This is a
source-supported mechanism proposal, not an executed proof of no disk/kernel
amplification. Root still records actual wait/core flag and relevant filesystem
and resource outcomes. [Linux core manual](https://man7.org/linux/man-pages/man5/core.5.html),
[per-process dumpability](https://man7.org/linux/man-pages/man2/PR_SET_DUMPABLE.2const.html),
[Linux6.18 coredump source](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/coredump.c).

Alternative self SIGKILL cannot generate a core dump and is used by existing
managed_reap; it would exercise signal termination rather than the desired
SIGSEGV path. A Rust panic or abort path is unnecessary. The proposal uses actual
wait results because dropping Child does not terminate or reap it; ExitStatusExt
provides signal/core/raw data without inventing an exit code from128+signal.
[Rust Child ownership](https://doc.rust-lang.org/std/process/struct.Child.html),
[Unix wait-status access](https://doc.rust-lang.org/std/os/unix/process/trait.ExitStatusExt.html).

## Capture, neighbor and history evidence

Keep the existing files-only declaration0, crash-parent AGENT1 and healthy
HARNESS2. Member1's inherited fd1/fd2 contain bounded deterministic child data
and separately framed parent wait/progress reports. The capture envelope remains
**member/channel provenance**, not arbitrary exact child/tool attribution.
Namespace PIDs in the parent report are not equated with the VM-global PIDs in
kernel history without a separately established mapping.

After all child rounds and the exact parent final pair, require canonical member1
End/terminal offsets and QUIC EOF, then reuse the existing owner release gate for
new exact sibling2 stdout/stderr production. Independently check main progress
before and after release. Retain canonical bytes, metadata before/after stop,
run removal, observer joins and exact FD-return evidence before strict terminal
assertions. The normal parent's metadata requirement remains ended/tail complete;
the existing normal-exit root finding takes priority over this new fixture.

Read the existing `shards read processes` once after progress and again after
stop, and preserve raw private `processes.*` segments before run removal. Compare
only actually observed member/exec round-marker/fork/exit correlations. Existing
`init::procevents` documents lost/unrecorded records, omitted threads and fallback
cmdline/comm/gone provenance; daemon retention rotates segments. Missing,
unsupported, truncated or lost history remains Unknown. Neither no-loss markers
nor source expectations prove complete kernel history, mandatory enforcement or
an exact writer identity. No product recorder changes are proposed.

Owned future paths after approval: narrow testguest workload.rs extension,
tenant_exit family/new fixture module and this evidence directory. Host parent,
allocation header/module registration and cost fixture stay root/resource-owned
unless root explicitly approves the one task-envelope wiring change (now approved).
Scoped
compile-only GNU/musl checks may use a fresh private source/target at at most
1CPU/1GiB/2GiB after headroom; every actual helper/guest/negative/mutation/resource
campaign remains coordinator-owned and unrun here.
