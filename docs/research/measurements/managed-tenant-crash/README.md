# Owned-child crash fixture: implementation and light evidence

Implementer checkpoint, not a real-VM result or lane completion. ROOT owns all
audits, profiling/benchmarks, actual helpers, VM/E2E, mutations, chaos/stress,
resource/population campaigns, seven-target combined gates and main merges.
No helper/test body, socket, namespace, BPF, VM or intentional fault was executed
by this lane. No production behavior was changed or bug claimed reproduced.

## Identity, policy and ownership

POLICY_READ IMPLEMENTER, Kiln agent01a1c5d28cea74a5b0241200190d6c54; coordinator
01a148884a4770fdaf0111f3f792a248. Full canonical managed-main-landing AGENTS.md,
R01–R16, development-evidence.md, CLAUDE.md, managed-runtime-plan.md and
`/Users/adalundhe/g121-handoff.md` read before edits. Initial main24f48ee5; ROOT
later reported main096c9c7be1752a5eaf99eda2c7eb4723c2a8f6db. Policy files unchanged
fromf5cec365; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Source branch p/managed-tenant-crash starts at signed proposal
f2da72b2b823db05b175e7f737f380e737b8978d, preserving signed normal-exit
fce89fb1063b5c2344f920ff9e967c023ce3abfc on its own branch.

Owned changes: managed-capture branches in workload.rs, tenant_exit.rs family,
this evidence and the queued observer/stdio proposal records. Host parent change
is ONLY the approved optional positive-u32 AGENT task argument/validation/line;
existing normal/default callers passNone. No header/module/global allocator,
cost fixture, production security/trust/crypto, host settings or ROOT target edits.
No subagents, main merge, force push, signing waiver or protected cache deletion.

Persistent lane goal bd4367f47f7b82b4a4a7432bcd57cacf and plan
01a1c5d33266732a9671b3f954e81989 remain open through ROOT closure/release. A signed
checkpoint is progress. Subsequent queue contracts are
[observer reconnect](../managed-tenant-observer/PROPOSAL.md) and
[inherited output/stdio](../managed-tenant-stdio/PROPOSAL.md); required host seam
and new helper mechanism approvals remain explicit dependencies.

## Approved behavior and state

ROOT approved [the bounded proposal](PROPOSAL.md), the task argument extension,
then the private one-byte stdin ordering amendment on2026-11-10. The parent checks
positive finite count/work/cleanup inputs, checked representable absolute
work_until and cleanup_until=work_until+cleanup, and the successful controlled-byte
upper bound plus two fixed failure reports against caller REPORT_BYTES BEFORE
spawning. It uses fresh std::process::Command with literal argv, private child-read
stdin and inherited fd1/fd2. No shell, raw fork, pre_exec or parallel manager.

Round states are Spawned→HeldPidfd→Released→ExitReady→Reaped. At most one owned
unreaped Child exists. Only a successful held pidfd permits the one-byte release;
the original work deadline is checked before release, and the writer closes
promptly. Child reads that byte and closes fd0 before any suppression/fault. EOF,
wrong byte or failed close is a typed pre-fault failure. Child then sets AND reads
back CORE soft/hard0, sets AND reads back dumpability0, installs AND verifies
default SIGSEGV, unblocks AND checks its signal mask, writes the exact controlled
binary pair, then raises SIGSEGV on itself only. Any prerequisite refusal returns
failure before that intentional signal; grants/syscall profiles never broaden.

Parent waits for its held pidfd using the existing absolute-deadline ppoll helper,
then obtains actual Child.wait status. It emits a fixed report with raw Unix wait
status, signal Option, exit-code Option and core flag after each actual reap; only
SIGSEGV/None/no-core permits the next round. The final parent pair follows all
reaps. Error paths close any unreleased stdin writer, target only the unreaped
owned Child, and attempt pidfd readiness plus actual wait under the ORIGINAL
cleanup deadline. If cleanup cannot establish reap, report Unknown and admit no
next child. Dropping Child is never called a reap. Blocking kernel/spawn/output
operations are not advertised as universally hard wall-clock bounded.

The fixed test-only stdout wait frame is magic NUL-W1-NUL, u32BE round, i32BE raw,
two tagged Option<i32BE> values (signal then code), and u8 core flag. Stderr failure
frame is magic NUL-E1-NUL, typed stage, two typed causes with separately tagged
optional i32BE errno (original then cleanup), cleanup state and raw reaped status
when available. Stages/causes are explicit enums in the helper; non-OS EOF/deadline/
input failures do not collapse into an absent errno. These frames are fixture
data, not a new product protocol or trusted tool provenance.

The host validates TASKS as positive u32 against the ACTUAL
`agentfile.rs::domain` grammar (`--processes=<n|none>`, numeric n>0). It supplies no
default/reservation guess and claims no hard aggregate/parent/kernel bound. Count,
two channel payloads, decimal round-width upper bound and fixed report sizes
derive the test byte bound. Each test number is caller input, ABI size, native
signal/FD constant or defined fixture framing; none is a production policy number.

Member0 remains files-only; AGENT1 is the parent/children; HARNESS2 waits on the
existing declared NETWORK/EXPOSE/CONNECT owner gate. The host preserves each raw
canonical record BEFORE exact-byte/state assertions. It requires initial/child/
actual-wait/final member1 bytes, exact canonical End and transport EOF on both
channels before releasing member2 for new exact stdout/stderr. Main exec progress
is independent before and after release. The shared normal-exit path preserves
its existing strict ended/tail-complete requirement; member2 stopped by its owner
remains aborted/tail-unknown. No metadata assertion is weakened to make it pass.

Existing read-processes snapshots after progress and stop plus private raw
processes.N segments are retained before run removal. Missing/unsupported/lost/
rotated/short-lived/thread history is Unknown; only actually present member/exec
round-marker/fork/exit facts can correlate. Namespace PID is never equated with a
VM PID. Inherited output stays member/channel provenance; fixture reports do not
establish arbitrary exact writer attribution or a complete kernel population.

Owned stop, inspect false0, raw stopped metadata, removal, daemon stop, observer
joins and exact FD-return guard happen before terminal assertions. Those witnesses
still do not prove complete population/reaping, kernel resource return or actual
daemon/VM exit causality; ROOT owes its held-watch/population/resource evidence.

## Primary sources and alternatives

Pinned in-repo seams: AGENT grammar; existing Fixture/Observer/managed_capture;
actual ABI Record/StreamState; native managed_reap pidfd/wait pattern; existing
managed_limits explicit ingress grammar; procevents/processes retention semantics.
Reusing these avoids a shell, a new process manager, a recorder or extra authority.
SIGKILL is a core-free alternative, but exercises a different termination path.
Normal exit remains the preserved prior fixture. Panic/abort is unnecessary.

[Rust Child ownership](https://doc.rust-lang.org/std/process/struct.Child.html)
requires explicit wait/cleanup;
[ExitStatusExt](https://doc.rust-lang.org/std/os/unix/process/trait.ExitStatusExt.html)
distinguishes signal/core/raw wait status from shell128+signal exit codes.
[pidfd_open](https://man7.org/linux/man-pages/man2/pidfd_open.2.html) describes held
process readiness and unreaped-child identity assumptions.
[Linux dumpability](https://man7.org/linux/man-pages/man2/PR_SET_DUMPABLE.2const.html)
and [Linux6.18 coredump_skip](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/coredump.c)
support the verified child-local suppression sequence; RLIMIT_CORE alone is not
a piped-handler guarantee. This is source support, not an executed amplification
or filesystem/resource proof. Linux guest raw wait/signal expectations use libc
constants and [Linux6.18 exit](https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/exit.c)
and [signal](https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/signal.c)
semantics, not a portable POSIX raw-status assumption. ROOT checks actual results.

## Light checks and corrections

`prepare-light.py` copies exact helper/host fixture bodies and actual ABI sources/
build.rs into a small compile-only crate. Surrounding Fixture/Observer/common glue
remains explicitly inert stubs from the earlier normal-exit harness. Actual host
start/task wiring, dispatcher, carried helpers, grants and product integration are
NOT validated by those stubs. The helper scope additionally denies unwrap/expect/
panic/indexing/string-slicing clippy hazards. No helper/test binary is executed.
Direct libc/serde_json versions come from the workspace lock; retain the separate
generated light Cargo.lock without calling it the full workspace dependency gate.

All light commands use Kiln admission linux/amd64,cpus=1,mem=1g,disk=2g, NEW private
Fedora source/target under ~/managed-tenant-crash-light-01a1c5d2-vN. Admission is
headroom, not measured hard aggregate accounting. No local build/compile or new
full worktree. Mac fell below30GiB, all local builds stayed held, and ROOT later
reported floor restoration; only small source/transfer snapshots were written.
ROOT Fedora ~/managed-capture-check/{source,target}, ~/managed-capture-target,
ROOT Mac seven-target sources/caches and paused Windows ARM remained untouched.

Recorded iterations are revision-specific:

- v1 background dispatch r1 never supplied runner/output and independent read
  initially found no remote directory. It was preserved, never killed; no check
  is claimed from its empty status. v2 used the ordinary run entrypoint with a
  fresh private path after that concrete dispatch failure.
- v2/r3: format0/policy0; GNU/musl check/clippy101 for three Rust temporary lifetime
  errors in the new shared host path. Corrected explicit closure argument type
  and named harness payload lifetime. Raw failures retained.
- v3/r6: all four compile/clippy0 plus format/policy0, exact PRE-AMENDMENT source.
  This result does not validate the later private release gate.
- v4/r8: gate amendment compile/clippy0 on GNU/musl, format/policy0. Later typed
  non-OS-cause reporting change is separate source and needs its own final gate.
- v5/r9 FINAL helper/host-fixture bodies: GNU check0/clippy0, musl check0/clippy0,
  format0/policy0. Exact checked helper-functions SHA256
  `8368f5293a0f3b9c9111de7dc58cdce941190a7c7155094ffa3d8e2194082d34`;
  host fixture SHA256
  `c8f6df9bf8d536f7c1a80338447f99d4313d4835623641c04271bcd1d9624315`.
  Local source bodies were checked byte-for-byte against the remotely compiled
  hashes AFTER adopting the repository-configured formatting. The strict normal
  terminal assertion remains byte-identical to fce/f2. See source.sha256,
  raw/v5/checked-hashes.json and raw/v5/SOURCE_MATCH.json. Source388KiB;
  private target83,800KiB. These are disk observations, not runtime cost claims.

Exact v5 compile-only commands with CARGO_BUILD_JOBS=1/private CARGO_TARGET_DIR:

```sh
cargo check --tests --offline -j1
cargo clippy --tests --offline -j1 -- -D warnings
cargo check --tests --offline -j1 --target x86_64-unknown-linux-musl
cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings
python3 policy/scripts/check-development-policy
```

`kiln-v5.command` retains the executed runner command with the source transfer
payload replaced by its hash. `prepare-light.py REPO NEW_PRIVATE_SOURCE` recreates
the exact minimal compile source; the command records the copied policy files,
repository formatting and original host glue (the latter remains uncompiled).
Raw text/log/environment/lock/transfers stay under raw/. Large JSON through the
ordinary run tool was truncated; transfer was corrected using code_run's larger
return envelope and SHA256-verified before local writes. No check was rerun for
that read-only evidence-transfer correction. [Primary-source research with its
machine-generated grades](RESEARCH.md) is separate from actual effect evidence.

One implementation-scope correction was reported immediately: initial remote
formatter lacked repository max_width110 config and its full-host copy introduced
unrelated whitespace in imports/Client/other functions. Reconstructed the host
from clean HEAD preserving ONLY authorized task wiring; no reset/stash/discard.
Final formatting carries repository rustfmt.toml and checks exact host diff scope.

## Owed ROOT gates and repeatable invocation

Root wires the preserved tenant_exit module declaration centrally, preserving the
resource lane's header/allocator/cost ownership. Build actual matching host/guest
and carried helpers together on the combined source; record all artifact hashes.
Normal-exit actual gate comes first. No production terminal fix without its exact
live red. Then root chooses finite count/tasks/deadline/report/port values and runs:

```sh
SHARDS_TENANT_CRASH_COUNT=CALLER_POSITIVE_U32 \
SHARDS_TENANT_CRASH_TASKS=CALLER_POSITIVE_U32 \
SHARDS_TENANT_CRASH_WORK_MS=CALLER_POSITIVE_U64 \
SHARDS_TENANT_CRASH_CLEANUP_MS=CALLER_POSITIVE_U64 \
SHARDS_TENANT_CRASH_REPORT_BYTES=CALLER_ENVELOPE \
SHARDS_TENANT_CRASH_PORT=CALLER_NONZERO_U16 \
SHARDS_TENANT_CRASH_DEADLINE_MS=CALLER_POSITIVE_U64 \
SHARDS_TENANT_CRASH_RECORDS=NEW_ABSOLUTE_DIRECTORY \
SHARDS_TENANT_CRASH_SOURCE_REVISION=EXACT_COMBINED_REVISION \
cargo test -p shards --test managed_output --profile test-release \
  tenant_exit::owned_child_crashes_preserve_sibling_and_main_progress \
  -- --ignored --exact --nocapture --test-threads=1
```

ROOT also supplies existing kernel/init/helper selection. It owes actual grants/
syscall compatibility, pre-release refusals, suppression/core/filesystem effects,
wait-status rounds, exact bytes/offsets/normal terminal preservation, independent
neighbor/main progress, history limits, finite failure cleanup and complete owned
population/resource return. Mutation/negative cases must expose actual wrong
bytes/status/cleanup/ownership effects, not just lose an alert. Seven-target and
supported KVM/HVF/WHP/distribution/libc gates, all audits/profiles/benchmarks remain
unrun here. No skipped/unavailable/ignored case or old revision becomes a pass.

## R01–R16 accounting

| Rule | Scoped proof and remaining gap |
| --- | --- |
| R01 | Production unchanged; new helper returns typed failures and scoped clippy denies panic hazards. Host assertions are test-only. Allocation/implicit panic proof is not claimed; ROOT audit owed. |
| R02 | Exact approved fixture mechanism, primary seams/alternatives above; no architecture or product fix. ROOT actual compatibility/effects owed. |
| R03 | Caller inputs, native/ABI sizes and fixture framing derive limits; no picked production constant/default or aggregate proof. |
| R04 | Explicit owned round states, typed stage/cause/cleanup, actual ABI capture states and exact offsets. ROOT actual transition/refusal/mutation proof owed. |
| R05 | Existing Fixture/Observer/ppoll reused; one private one-shot stdin readiness handoff, fixed deadlines, no periodic probes/cache/new manager. ROOT affected-path audit/cost owed. |
| R06 | Scoped helper/family changes and light clippy; ROOT combined complexity/source review owed. |
| R07 | Comments explain ownership, pidfd reuse, core-handler exception and release/cleanup ordering. |
| R08 | No comparative speed/resource or4–5× claim. ROOT baseline/benchmark work remains open. |
| R09 | No product fix/reproduction claim; normal terminal concern waits ROOT actual red logs. |
| R10 | Repeatable source/commands, failed/successful compile records and hash transfer retained. Live red/green/mutation/resource evidence owed. |
| R11 | GNU/musl x86_64 compile-only scope; actual seven-target/real-host matrix unverified here. macOS ARM64 only; Windows ARM paused. |
| R12 | Shared existing seams/native program, no installed binary/OS adapter/host privilege/trust/crypto change; child-local suppression explicitly approved. ROOT portability/effect proof owed. |
| R13 | Narrow fixture family and tiny task parameter; no product abstraction/parallel manager. |
| R14 | No actual profile, allocation/fault, kernel/parent accounting or leak-free claim. ROOT owns all measurements. |
| R15 | No children/audits/heavy checks/main merge; signed named-path own-branch checkpoint/push and owed ROOT runs. Lane goal remains open; continue approved queue. |
| R16 | Private small admitted sources/targets, no local compile/new full worktree/cache deletion; evidence/protected jobs/paused VM preserved. ROOT owns disk recovery. |
