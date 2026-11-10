# Finite managed-tenant task refusal fixture

This is a fixture implementation for G10/P2/P12, not evidence of a product defect
or a passing exhaustion campaign. The coordinator owns the actual VM run,
mutations, repetition, audits and parent/kernel resource measurements. This branch
starts at combined `4d3f454f194a0b6927423f0f859071147c43786d`; no production init,
pids policy, guest security, runtime dependencies or installed deliverables change.

Policy readback: implementer, canonical main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
AGENTS, all R01–R16 and evidence were read at that revision; CLAUDE, full managed
plan and current g121 handoff were read before implementation. The earlier c921 /
1ed0 receipt is superseded for role policy. Root alone owns heavy validation;
this lane only implements and runs targeted unit/compile checks. No child agents.

## Authority and observable accounting

`AGENT --processes=N` accepts a nonzero u32 in
`agentfile::agentfile::domain`, maps without an added actor allowance through
`init::domains::domain`, and writes literal N to the tenant's `pids.max` before
launch (`domains.rs:1625` at the base). `none` selects the existing no-process
seccomp policy; it is not numeric zero. An omitted value can inherit artifact asks.
This test declares a numeric total and never adjusts it from an observation.

Current source predicts four idle tenant tasks: shards-server, its persistent
sandbox helper (`server::run`/`spawn_helper`), namespace PID1 protected reaper,
and namespace PID2 actor (`managed::Launch::child`). The server enters the same
tenant cgroup before spawning its helper (`domains::Server::start`). The PID
controller counts threads, not just process leaders. This prediction is an
explicit **fixture input**, not an observed entitlement guarantee.

The host takes exactly one trusted `top -o pid,ppid,uid,nlwp` snapshot at each
barrier. It checks the actor's UID and root-owned parent, both server UID rows
and their parent relation, and each process's real reported thread count.
A mismatch fails before fill; labels alone cannot satisfy this check. At refusal,
children must be immediate children of the observed actor with the same UID.
After recovery the same observable tree must contain zero actor children.
Actor records also report namespace PID/PPID, real UID, `/proc/1` owner and Threads.

Direct tenant `pids.current`, `pids.max`, `cgroup.procs` and membership are
**unavailable through this fixture's public observation seam**. Main's cgroup
namespace is rooted at its own workload, and actors cannot see ancestor objects.
No new visibility, control FD or security grant is added to read them. A process
snapshot is not direct cgroup membership and never grants admission. Root may
retain existing trusted pids refusal events, but this test does not parse alerts
to decide success. Actual fork EAGAIN has several possible kernel causes; without
additional trusted observation it alone does not uniquely attribute the cause to
this tenant controller. The declared mapping, held population and successful
replacement after release are the bounded runtime experiment.

## One finite cycle and ownership

All inputs must be supplied: `MANAGED_LIMIT_TASKS`, `MANAGED_LIMIT_RESERVED`,
`MANAGED_LIMIT_CHILDREN`, `MANAGED_LIMIT_ROUNDS`, `MANAGED_LIMIT_PATIENCE_MS`.
Tasks must equal checked reserved+children; both components must be positive.
This fixture accepts exactly one round. Root chooses repetitions externally;
there is no fork-until-failure loop or automatic retry. The example total6 is
source prediction4 + two explicitly selected held children, not runtime policy.

The supplied native testguest AGENT and HARNESS use ordinary root manifests and
scoped volumes. Two OS-assigned ports are explicitly declared through existing
NETWORK/EXPOSE/CONNECT ingress grants. They expose synthetic fixture controls,
not backend descriptors. No shell supervises the actor. Host-to-guest control uses
held TCP connections, avoiding unproven remote-filesystem inotify behavior.
Guest-to-host startup/publication uses the existing host FileWatch seam.

1. Actor binds its control listener and publishes typed Ready. Host checks actual
   idle process/thread observations before sending typed Fill.
2. Actor reserves its finite ownership vector, then creates exactly the admitted
   children. Each child has its own held socket gate, announces actual readiness,
   and blocks on that gate. Parent holds a pidfd opened before releasing it.
   The fork child executes only raw descriptor syscalls and `_exit`; it closes
   inherited descriptors except its own gate. It cannot retain sibling gates.
3. Actor makes **one** additional fork attempt. Only the typed error from the
   fork syscall with actual EAGAIN counts. Socket, allocation, pidfd or readiness
   errors fail separately. Unexpected successful admission is held, ended and
   reaped before reporting failure; it cannot launch another generation.
4. Actor publishes typed Refused while admitted children stay held. Host verifies
   the observable population and sends a newly created 128-bit synthetic nonce to
   the healthy HARNESS and a separate main exec. Both must answer exact bytes/text
   after this refusal barrier.
5. Typed Release lets each owned child exit. Parent waits on its pidfd, calls
   waitpid on that exact child, checks successful exit and then ECHILD without
   creating another child in between. It performs no waitpid(-1) sweep.
6. One replacement child must start, signal ready, release and be reaped through
   the same path. Recovered carries held0, children+1 actual reaps and replacement
   PID. The actor's real open-descriptor count must return to its post-connect
   baseline; the host also checks the live-held delta of two FDs per child
   (one gate and one pidfd). The transient socketpair for the excess attempt is
   dropped before that snapshot; it is not a claim that all kernel cost is charged.
7. HARNESS and main answer new nonces after recovery. Actor/HARNESS remain blocked
   on their control streams so host stop actually cancels live actors. Host checks
   the daemon's stopped/PID0 state, removes its private run and stops its private
   daemon. These control results are **not** proof of host process/FD/kernel return;
   that separate coordinator gate remains owed.

A child ownership object kills via its held pidfd and waits only for its own child
on every error/drop before successful reaping. Kernel-uninterruptible wait limits
remain an OS limitation, not a guaranteed hard teardown bound. No reader/helper
threads are created by this workload, no process names confer authority, and no
other tenant or host process is signaled. The test's private run is stopped on
host assertion/error cleanup through existing common test supervision.

Wire records are exactly sixteen big-endian u32 words: version, typed phase,
budget, observed facts, typed cause and errno. Unknown version/phase/cause/command
and wrong length reject explicitly. Phase/cause types remain separate from human
stderr diagnostics. Public constants describe only this test schema. The raw
child's exit codes and gate bytes are fixed synthetic control choices; production
policy constants are untouched.

## Waits, alternatives and primary evidence

The Linux PID-controller contract specifies TID accounting and refused fork/clone
at the inherited hierarchy limit: [kernel cgroup-v2 PID section](https://docs.kernel.org/admin-guide/cgroup-v2.html#pid).
The manual distinguishes fork EAGAIN from ENOMEM and lists several EAGAIN causes:
[fork(2), Linux man-pages6.19](https://man7.org/linux/man-pages/man2/fork.2.html).
Host checks use Linux guest EAGAIN11 from the [pinned Linux6.18 errno ABI](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/asm-generic/errno-base.h), never the host libc value (Darwin differs).
A held pidfd supplies exit readiness; it is not a reaping operation:
[pidfd_open(2), Linux man-pages6.19](https://man7.org/linux/man-pages/man2/pidfd_open.2.html).
Actual reaping and ECHILD use [wait(2)](https://man7.org/linux/man-pages/man2/wait.2.html).

The existing `managed_reap` fixture at the base is the established in-repository
reference for pidfd readiness and finite absolute deadlines. Direct child gates
are enough here because all synthetic children are immediate owned children;
no new process manager, namespace, supervisor executable or task ledger is needed.
PID scanning/retry sleeps would obscure the exact held population and make the
refusal boundary ambiguous. `Command::spawn` would require another executable
startup/descriptor protocol and does not add useful coverage for this specific
fork boundary. A thread-limit experiment is separate owed coverage: this fixture
chooses fork and never claims pthread/clone3 equivalence from a label.

There are no caches. Every file check follows a registered host FileWatch and a
completion notification. Socket accept/read, child-ready gate and pidfd exit waits
have one absolute caller deadline; interruptions retry the operation, not a
periodic probe. FD and process counts are barrier observations, never admission
state. Profiling/allocation/copy/kernel and unfairness costs remain unmeasured.

## Light checks and exact coordinator invocation

Final checked source is `6d00dd0c3341d8fd8b81121a0fc4a7e2cad6d67a`.
Fedora44 amd64 / kernel6.19.10-300.fc44 / Rust1.98.1(48a229cea), LLVM22.1.8;
identity was captured in the private repository cwd before each proof. Actual
Linux-GNU guest workload and host fixture strict clippy passed. These are compile
and pure units, not a carried-musl executable or live VM result.

| Kiln run / retained directory | Source | Observed light result |
| --- | --- | --- |
| r47 / `e98b2e24-light` | e98b2e24 | record2/0, host tree1/0; both clippy101 on two fixed-chunk codec lints. |
| r49 / `fixed-chunks-light` | 2ec73275 | record2/0, host tree1/0, guest clippy0; host clippy101 on unused old file control, expect and stderr diagnostics. |
| r50 / `agentfile-light` | eb198544 | record2/0, host2/0, guest+host clippy0. This parser/check test did not yet assert effective network boundary. |
| r51 / `owned-birth-light` | 3fec1300 | record2/0, host2/0, guest+host clippy0 after exact birth cleanup follow-up. Same incomplete network-boundary assertion. |
| r53 / `boundary-red` | a4a743ef | record2/0; host1pass/1fail (exit101), actual boundary[]; guest+host clippy0, wrapper1. |
| r54 / `boundary-green` | 6d00dd0c | record2/0, host2/0 including effective boundaries; guest+host strict clippy0, wrapper0. |

All84 raw identity, source-hash, command, log and exit files remain verbatim in
[light-evidence](light-evidence/), with SHA256SUMS. The first14 files predate the
durable driver and carry equivalent plain command/identity text; subsequent runs
use `run-light.py`. Archive transfers were SHA256-verified. Source touching forces
actual recompile after sync; no stale binary result is reused. Final source hashes:

- guest actor `da515c695857a3870d08c6f49771a659867fee170f463d3289cb435eedf40171`
- shared codec `241c493ee25820a2c31daba97f088eaa64e7d521f9d062a1cbd368033606fc7a`
- host fixture `f8a1f5e741d47f72cb73b4180ee78ce732e2a557ac3265bbccbd4378d5bc296c`

The first constructor used `NETWORK --ingress=7100,7101 control`. The parser,
instructions parser and `agentfile::check` returned Ok, preserving one raw ingress
port string `7100,7101` in normalized Network.ports. The added actual boundary
assertion reproduced **no effective ingress**, `[]`, against expected
`[(6,7100,7100),(6,7101,7101)]`. The corrected constructor uses repeated
`--ingress=7100 --ingress=7101`; its native assertion observes exactly that vector
and empty outward boundary. Historical comma source/raw logs remain intact.
This fixes the fixture's declaration. Accepting then dropping a malformed port
request is a separate coordinator-owned checked-plan validation question; no
production parser/grant fix or networking product failure is asserted here.

The original `scripts/check-development-policy` first failed because this sparse
checkout omitted `.github/workflows/ci.yml`; adding the existing tracked `.github`
(and required vendor sources) to sparse checkout made it pass. No policy was
weakened. Final rustfmt/diff/policy checks pass. Commits use signed named-path gits
and the required exact attribution; signed headers/trailers were checked, not a
certificate trust chain.

Light replay in the owned clone with the unchanged4CPU8GiB10GiB envelope:

```sh
CARGO_TARGET_DIR="$HOME/managed-limits-target" \
python3 docs/research/measurements/managed-limits/run-light.py \
  --evidence "$HOME/managed-limits-evidence/reviewed-light"
```

That path must not already exist. It sets SHARDS_HELPERS=skip, runs exactly the
four light commands and never invokes the ignored VM test or actor. The final
remote clone is `~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-tenant-limits-d0af2755ad`,
target `~/managed-limits-target`, external evidence `~/managed-limits-evidence`.
No job remains active; original endpoint target was copied read-only after an
empty open-FD check and remains preserved. This lane made no local builds.

No real VM, task exhaustion, chaos, sweep, benchmark or profiling run was performed.

From the reviewed/composed source, in a coordinator-owned private clone/target
with carried helpers, KVM/HVF/WHP and the ordinary 4CPU/8GiB/**20GiB** VM envelope:

```sh
env -u SHARDS_HELPERS \
  MANAGED_LIMIT_TASKS=6 MANAGED_LIMIT_RESERVED=4 MANAGED_LIMIT_CHILDREN=2 \
  MANAGED_LIMIT_ROUNDS=1 MANAGED_LIMIT_PATIENCE_MS=120000 \
  CARGO_TARGET_DIR="$HOME/managed-limits-vm-target" \
  timeout 600 cargo test --locked -j4 --profile test-release -p shards \
  --test managed_limits \
  a_finite_tenant_task_refusal_recovers_without_blocking_its_neighbors \
  -- --ignored --exact --test-threads=1 --nocapture
```

The 120000ms is an explicit initial experiment input, not a production timeout or
measured guarantee. Kernel/artifact/build startup is separately bounded by common
CLI supervision; actor/control work uses one deadline started with its run, and
partial records never extend it. On any failure retain full raw stdout/stderr,
daemon log, exact source/binaries, toolchain/kernel/arch and input environment.

Owed root gates: actual refusal/recovery at the declared boundary; real healthy
neighbor/main nonces after each barrier; cancellation while children remain held
and ownership-object error paths; independent limit/hold/reap/check mutations;
stop/daemon-owned host tasks/FDs and parent/kernel cost return; repeated campaigns
under root-selected finite envelopes; thread/clone alternatives; supported seven
target/full real-VM KVM/HVF/WHP and major distro/libc coverage. Windows ARM VM stays
paused. A skip or source prediction is unverified, never a passing VM result.

## R01–R16 evidence accounting

| Rule | This batch / remaining proof |
| --- | --- |
| R01 | No production code changes; checked test record/budget, fallible finite reserve, raw fork child safety. Focused compile/clippy below; no universal panic proof. |
| R02 | Existing managed_reap/server/launch seams plus primary PID/fork/pidfd/wait contracts above; actual VM behavior owed root. |
| R03 | All workload counts/deadline/rounds explicit fixture inputs; total equals reserved+children. Protocol literals only, no runtime policy change. |
| R04 | Typed Budget/Phase/Cause/Command/Invalid and internal Failure; unknown tags rejected, no alert text controls success. |
| R05 | Owned socket/gate/pidfd/FileWatch completion; single authorities, no cache or periodic probes. Common CLI supervision is existing, not a new claim. |
| R06 | One actor cycle, one excess attempt, exact owned children, two nonce barriers; no recursive creation. |
| R07 | Safety/ownership/deadline rationale comments; source links and bounds recorded. |
| R08 | No comparative performance claim;4–5× target unmeasured and owed coordinator. |
| R09 | No reproduced product defect asserted and no production fix attempted. Actual exhaustion proof owed coordinator. |
| R10 | Sources, light commands/logs/identity retained below; actual effect/mutations/cleanup logs owed coordinator. |
| R11 | Linux-target light checks below only. Seven-target and real supported host VM matrix unrun; macOS arm64 only, paused WinARM preserved. |
| R12 | Existing public CLI/grant/common platform readiness; testguest is already carried, no new install/root/security/crypto dependency. |
| R13 | One finite fork case; no generic manager, retry engine or broad cleanup. |
| R14 | All profiling/benchmarks/parent+kernel accounting root-owned; no fast/leak-free/allocation-free claim. |
| R15 | Scoped implementation/light tests only, signed named-path checkpoint, no children or main merge; all heavy gates returned. |
| R16 | Private clone/target4CPU8GiB10GiB light envelope, original inactive endpoint target preserved read-only; Mac >30GiB, no local builds or cleanup. |
