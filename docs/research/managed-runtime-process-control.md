# Managed agents: Linux process authority, interception and telemetry

Research checked 2026-11-10 UTC (2026-11-09 Pacific), against Shards revision
`ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8`. This is research and a proposed design, not
an implementation or evidence that the proposed tests pass. No builds, installations,
VM runs or performance measurements were performed for this note.

**Evidence labels:** **Documented** means an upstream interface contract or the
project's own technical documentation; **source inspected** means the cited code was
read; **proposal** means a Shards recommendation requiring implementation and tests;
**unverified** marks a property not established here. Product documentation describes
its design, not independent validation of an overhead or security claim.

## 1. The authority must survive an uncooperative actor

**Recommendation:** deepen Shards' existing native guest-domain implementation. Give
each declared AGENT/HARNESS a supervisor-owned lifecycle, kernel-enforced resource and
reachability boundary, private output endpoints, and independently authenticated
broker interfaces. Apply the boundary before the first untrusted instruction. Every
descendant inherits it regardless of fork depth, session changes, language, process
name, or whether it reports being a subagent. Process lineage is useful history;
membership in an immutable-to-the-actor confinement domain is the authority.

Do not depend on cooperation, a harness hook, a shell wrapper, a process-name match,
the actor's claimed PID, or an event arriving before the next action. Treat children
and all bytes they produce as hostile: they may close inherited descriptors, forge
events, saturate queues, attack parsers, retain resources, crash, or try every allowed
kernel operation. Authentication identifies a caller; authorization still checks the
specific operation against its domain's grants.

There are three different promises:

| Promise | Required authority | What does not establish it |
| --- | --- | --- |
| A process cannot reach a forbidden parent, sibling, file, device or network endpoint | Inherited kernel restrictions plus broker-owned capabilities | A process-start alert followed by a kill |
| Bytes crossing an authorized output/network/tool boundary are mediated | A trusted endpoint that owns the data path and applies backpressure | Merely observing `write`, `connect`, or a harness log |
| Every attempted action has complete, durable history before it can take effect | Synchronous admission/recording at the relevant boundary, with bounded failure behavior | An asynchronous connector/eBPF event ring |

These distinctions matter for both correctness and performance. A fast event sensor
cannot silently be promoted into a complete reference monitor. Conversely, existing
kernel policy need not round-trip through a userspace supervisor for every syscall.

## 2. What Shards already has, and the actual gaps

The following are **source inspected**, not a new audit of every escape path. Links
pin the inspected revision; line numbers are deliberately descriptive rather than a
claim that another branch has the same layout.

| Existing seam | Inspected behavior | Consequence for this work |
| --- | --- | --- |
| [`domains.rs`, `start` and `child`](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/crates/init/src/domains.rs) | Native `clone3`, `CLONE_INTO_CGROUP`, private mount/PID/IPC/network/UTS/cgroup namespaces, unique IDs, capability removal, `no_new_privs`, Landlock, seccomp, then `execve`. Child setup uses prepared data and kernel calls. | Extend this launcher; do not introduce Bash, `nohup`, `tee`, or a separately installed supervisor. The launched executable must remain the owned process. |
| Same file, `Started`, around lines 538–547 | Retains a label, cgroup path, output read FD and partial line buffer. It does not retain a pidfd in this struct. | A typed managed-domain handle with lifecycle state and stable leader identity is proposed, not already present here. |
| Same file, output pipe and child stdio, around lines 1767 and 2440 | stdout and stderr share **one pipe**; stdin is `/dev/null`; descriptors above 2 are closed before exec. | Capture is automatic, but original channel identity is lost. Separate output FDs are needed for typed stdout/stderr streaming. |
| [`run.rs`](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/crates/init/src/run.rs), domain output relay and `reap` | Polls domain output and relays prefixed lines onto the workload's stderr; uses `waitpid(-1, ..., WNOHANG)` to reap children. | Keep workload stdio separate from the new port stream. Byte framing, attribution and backpressure belong in the trusted relay, not in actor-supplied line prefixes. |
| [`procevents.rs`](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/crates/init/src/procevents.rs) | Kernel connector fork/exec/exit records; patched exec arguments; explicit `lost` and `unrecorded` results; threads excluded. Module explicitly says it is a record, not a security boundary. | Reuse its event path honestly. It neither blocks the action nor guarantees a complete thread/syscall/tool-call history. |
| [`hostbind.rs`](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/crates/init/src/hostbind.rs), module contract | Already uses seccomp notification for bind/listen compatibility; performs a bind itself using the copied address, rather than trusting a mutable target pointer. Explicitly not a security boundary. | Do not claim Shards never uses user notification. Its existing broker-performed-operation pattern is different from approving a target syscall and continuing it. |
| [`AGENTFILE_ARCH.md` §9.7–9.9](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/docs/architecture/AGENTFILE_ARCH.md) and [`architecture.md` D59/D60/D115/D123](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/docs/design/architecture.md) | Explicit grants, isolated in-VM server per domain, workload separated from agents, descendants constrained independently of their event records. | New local manifests must normalize into these same grants and checks. Changing artifact delivery must not weaken runtime isolation. |

The previous [rootless-security research](rootless-security.md) supplies the wider
host/VMM threat model. This note does not revalidate its benchmark numbers. Guest
kernel mechanisms work across KVM, Hypervisor.framework and WHP because the guests
are Linux; that does not establish successful runs on all those hosts. The trusted
guest init's setup privilege must not become host root, an actor's privilege, or a new
host installation requirement. Keep delivery as the single Shards binary carrying
its existing guest components.

## 3. Ownership, identity and descendants

### 3.1 Birth, identity and lifecycle

**Documented:** children normally inherit their parent's cgroup; `CLONE_INTO_CGROUP`
creates a child directly in a selected cgroup, eliminating the spawn-then-migrate
interval. `CLONE_PIDFD` can obtain its process handle at creation. Use those together
in the trusted launcher rather than racing `/proc` discovery after launch.
[Linux `clone(2)`](https://man7.org/linux/man-pages/man2/clone.2.html).

**Documented:** pidfds support polling for termination and signaling a particular
process. Opening a pidfd after fork has conditions concerning reaping and SIGCHLD;
creation with `CLONE_PIDFD` avoids relying on those conditions. A pidfd is not a
descendant collection and closing one does not terminate its target.
[Linux `pidfd_open(2)`](https://man7.org/linux/man-pages/man2/pidfd_open.2.html).

**Proposal:** keep a non-forgeable run/domain generation, cgroup directory FD, leader
pidfd, owned stream FDs and launch-error channel in a `ManagedDomain`. Expose methods
such as start, terminate, wait and subscribe; callers never receive its controlling
FDs. Use explicit states: prepared, starting, running, stopping, draining, ended, and
failed. A child-side setup/exec error must distinguish failed launch from an
application exiting with the same numeric code. Reaping has exactly one owner; a
subscriber cannot steal another caller's exit status. Labels and numeric PIDs are
presentation, never authorization. Restores/restarts receive new lifecycle identities
and revoke old broker sessions.

**Documented:** `cgroup.kill` kills the domain subtree and handles concurrent forks
and migrations; `cgroup.events: populated` describes remaining live processes.
Freezing is asynchronous and has a completion notification. Cgroup depth and
descendant controls bound cgroup creation, not process genealogy.
[Kernel cgroup v2 documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html).

**Proposal:** whole-domain termination uses the kernel subtree operation, then waits
for kernel-confirmed emptiness and child reaping, and finally completes output drain
according to the stream contract. A snapshot of PIDs followed by individual kills is
not equivalent under concurrent fork. Freeze before sensitive reconfiguration and
wait for the documented frozen state; a write returning is not that state. Keep the
domain a normal domain cgroup, not a threaded cgroup for which `cgroup.kill` fails.
No actor receives writable cgroup control files or a migration capability. Resource
limits are admission decisions: descendant count, threads, memory, CPU, storage,
FDs and queued bytes all need accounting, not just leader RSS.

### 3.2 Reparenting, PID 1 and crash behavior

**Documented:** a PID namespace hides ancestor processes from its children; its PID 1
adopts orphans, and its exit causes the kernel to kill the namespace's remaining
processes. PID 1 has special signal handling. Namespace nesting is limited, so do
not create one namespace per subagent depth: descendants can stay within one domain.
[Linux `pid_namespaces(7)`](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html).

**Documented:** a subreaper adopts orphaned descendants and can wait for them; it is
not inherited as a flag by a newly forked child. This is reaping support, not an
access-control boundary.
[Linux `PR_SET_CHILD_SUBREAPER`](https://man7.org/linux/man-pages/man2/pr_set_child_subreaper.2const.html).
`PR_SET_PDEATHSIG` concerns the creating parent thread, can miss an already-dead
parent, is cleared in forked children, and can be cleared by credential changes. It
is not a transitive replacement for cgroup/namespace ownership.
[Linux `PR_SET_PDEATHSIG`](https://man7.org/linux/man-pages/man2/pr_set_pdeathsig.2const.html).

**Proposal:** test `setsid`, process-group changes, double forks, intermediary exits,
and arbitrary ancestry depth without building a recursive userspace authority tree.
They must not escape the same domain. Clarify the lifecycle contract when the main
harness exits: stop its descendants, rather than silently adopting an independent
daemon. An actor currently becoming namespace PID 1 can fail to reap its children or
ignore ordinary termination signals. If compatibility requires a dedicated reaper,
implement it as native Shards code, account for its cost, and test it; do not assume a
generic CLI behaves like init. `waitpid`/`waitid` release a terminated child's zombie
resources; an exit observation alone does not reap it.
[Linux `wait(2)`](https://man7.org/linux/man-pages/man2/waitid.2.html).

## 4. Before-action enforcement: select the right kernel layer

| Mechanism | Established capability | Important boundary |
| --- | --- | --- |
| Namespaces, IDs and capabilities | Separate process visibility and access domains; remove authority before exec | A cgroup namespace alone changes the view, not the access rights to a leaked control FD. A user namespace is not required merely to describe the existing domain architecture. |
| `no_new_privs` | Inherited, irreversible prevention of privilege gains through exec | Does not revoke FDs or prevent all non-exec privilege changes. |
| Seccomp allowlist | Inherited syscall/ABI and scalar-argument restrictions | Does not dereference argv, pathname, `clone3` structures, or turn app semantics into kernel semantics. |
| Landlock | Stackable inherited access restrictions on supported objects and operations | Handle supported ABI/features explicitly; already-open capabilities remain important. |
| LSM hooks / BPF LSM | A policy at a relevant kernel security hook can return a denial | Must select a hook covering the actual operation, with supported kernel configuration; not every event hook can deny. |
| Seccomp user notification | Stop selected syscalls for broker handling | A copied pointer followed by `CONTINUE` is not a safe argument-based security gate. |
| Ptrace | Per-thread syscall and process-event stops and register control | Broad tracing adds scheduling/protocol work; observing one stopped thread does not freeze every writer of its memory. |
| Connector, tracepoint/eBPF event capture | Facts about executions/actions for history and detection | Delivery/processing is not authorization and may lose events. |

The table's first row describes the existing composition rather than a claim that
any single primitive is a complete sandbox. Kernel isolation also does not prove
safety against a guest kernel vulnerability. The microVM remains the boundary to
the rootless host; the trusted guest runtime remains exposed to hostile actor input.

### 4.1 Immutable inherited restrictions

**Documented:** `no_new_privs` survives fork, clone and exec and cannot be unset. It
blocks exec privilege gains from set-ID bits/file capabilities, but does not stop
non-exec privilege changes or receiving capabilities such as passed FDs.
[Kernel `no_new_privs` documentation](https://docs.kernel.org/userspace-api/no_new_privs.html).

**Documented:** seccomp filters survive permitted fork/clone/exec operations. Syscall
numbers and wrappers differ across architectures, and the syscall ABI must be
validated. Install the complete filter before launching untrusted multithreaded
code; later installation requires explicit thread synchronization.
[Linux `seccomp(2)`](https://man7.org/linux/man-pages/man2/seccomp.2.html).
The input is the syscall number, architecture, instruction pointer and six numeric
arguments. The kernel UAPI independently warns about continuing calls with mutable
pointer arguments.
[Linux 6.18 `seccomp.h`](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/seccomp.h#L48-L95).

**Documented at the pinned kernel generation:** Landlock restrictions extend to
future children. Linux 6.18 documents filesystem and TCP-port rules, ptrace
restrictions, and signal/abstract-Unix scopes. File descriptors preserve relevant
open-time rights; rules do not retroactively neutralize every pre-existing FD.
Use the 6.18 source, not newer documentation's additional network rights.
[Linux 6.18 Landlock documentation](https://github.com/torvalds/linux/blob/v6.18/Documentation/userspace-api/landlock.rst).
Not every filesystem operation is a Landlock-handled access right.
[Linux 6.18 Landlock UAPI](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/landlock.h).

**Proposal:** constrain namespace creation/joining, mounting, tracing, process-memory
access, pidfd-based FD extraction, BPF/perf, keyrings, privileged devices and raw
networking at the inherited policy layer. Preserve Shards' existing default-deny
network grants independently of Landlock: 6.18 TCP-port rules do not cover all UDP,
IP destinations, protocols or same-port peers. A missing required feature or failed
policy installation must refuse launch, not silently switch to observation-only.
Publish exactly which guarantees are enforced for that build/kernel/architecture.

### 4.2 Why inspecting argv then continuing is insufficient

**Documented:** seccomp user notification is intended for a supervisor to perform
operations on behalf of a target. Its `CONTINUE` response allows a target's mutable
arguments to change before the kernel uses them. Notification lifetime validation
also matters if a target exits or abandons its syscall. `ADDFD` supports returning a
broker-opened FD; this is a different design from approving mutable target memory.
[Linux `seccomp_unotify(2)`](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html).

**Proposal:** for a security-sensitive broker, copy a bounded typed request into
trusted memory once; authorize it against its authenticated domain; resolve/open
the object under the broker's fixed authority; perform the operation or return an
explicitly scoped capability. Do not authorize one string and then execute a target's
possibly changed string. Check cancellation before irreversibly granting resources
and close every unaccepted capability. A transparent exec broker is not equivalent
to a small typed tool service and has additional process identity/exec semantics to
solve. Existing `hostbind` compatibility brokerage is not evidence for a universal
security gate on arbitrary syscalls.

**Documented:** ptrace controls individual threads and can stop at syscall entry and
exit, with separate fork/clone/exec event options. Its state machine includes exits,
signals, and thread-group changes during exec.
[Linux `ptrace(2)`](https://man7.org/linux/man-pages/man2/ptrace.2.html).
**Inference:** a supervisor must not assume stopping one thread stabilizes all shared
memory or that every kernel action maps to a synchronous userspace call. Universal
ptrace supervision is therefore a costly and complex baseline candidate to measure,
not a justified default for Shards' low-overhead launch path.

### 4.3 Alternative operation paths: io_uring and descriptors

**Documented:** io_uring SQPOLL can submit and complete I/O without a syscall per
operation. Therefore syscall-entry inspection of `write`/`connect` alone cannot mean
every I/O operation is inspected.
[Linux `io_uring_setup(2)`](https://man7.org/linux/man-pages/man2/io_uring_setup.2.html).
Disabling new rings with the kernel sysctl leaves existing instances usable.
[Kernel `io_uring_disabled`](https://docs.kernel.org/admin-guide/sysctl/kernel.html#io-uring-disabled).
Restricted rings can be created disabled, given an allowlist of SQE operations/flags,
then enabled. This is a separately specified mechanism, not automatic seccomp
coverage of the ring's contents.
[Linux `io_uring_register(2)`](https://man7.org/linux/man-pages/man2/io_uring_register.2.html).

**Proposal:** retain the documented domain ban on io_uring unless a deliberate,
kernel-version-specific policy and coverage proof replaces it. Deny creation and
use paths; also prevent inherited/imported rings. Test alternate APIs rather than
assuming the program calls the libc symbol named in the policy.

**Documented:** Unix sockets can pass references to open file descriptions through
`SCM_RIGHTS`; connected-peer credentials reflect connection/socketpair creation
time, not a self-reported request field.
[Linux `unix(7)`](https://man7.org/linux/man-pages/man7/unix.7.html).
`close_range` can close the inherited descriptor range, with an unshare option for
shared FD-table races.
[Linux `close_range(2)`](https://man7.org/linux/man-pages/man2/close_range.2.html).

**Proposal:** give the child only its own data endpoints and explicitly granted
objects. No supervisor proc FD, namespace FD, pidfd, cgroup FD, device, connected
egress socket, broker listener or notification listener may leak through spawn,
exec, restart or snapshot. Separate control from actor output. Reject unexpected
ancillary FDs and close received FDs on every parse/rejection/cancellation path.
Do not add a raw connected socket handoff to bypass the required buffered relay.

## 5. Telemetry, semantic tools and output are different facts

The current D123 patch puts exec arguments into a kernel process event before the
new program runs, avoiding a later `/proc/cmdline` sampling race. Its capture remains
bounded and explicitly reports truncation/loss. It records process transitions, not
every attempted syscall or thread. An event existing in the kernel before exec does
not mean userspace durably recorded it before exec.
[Inspected `procevents.rs`](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/crates/init/src/procevents.rs),
[D123 and its kernel patch contract](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/docs/design/architecture.md).

**Proposal:** distinguish records by authority: kernel-observed process event;
broker-authorized tool request/result; relay-observed stream/flow; actor-reported
semantic annotation. Include lifecycle generation, domain identity, source kind,
sequence, truncation/loss status, and relevant operation/result. Preserve bytes
separately from presentation. Never let a forged stdout line create a process,
change a domain label, satisfy a tool approval, or manufacture success.

A Python tool, shell builtin, JavaScript function, JIT program or library call can
perform work within an existing process. Process execution telemetry cannot label
all those semantics. Similarly, TLS records do not reveal the remote tool request.
**Recommendation:** put enforceable tool operations behind Shards' typed, authorized
broker where feasible. Harness integrations can add semantic metadata but must not
be the only boundary. A mode allowing arbitrary native programs to use ordinary
filesystem/network APIs must describe its guarantees as object/flow confinement,
not universal semantic tool-call approval. Disallowing exec alone also does not
disallow code evaluation by an already-running interpreter.

Automatic stdout/stderr inheritance is useful but not an honest promise to capture
all possible output. A child can close/replace those FDs, retain bytes in memory,
write a granted file or communicate on another granted socket. Inherited descendant
stdio also does not identify the exact writer: multiple descendants can share the
same open pipe. Preserve trustworthy domain/channel identity; claim per-process
identity only where a trusted kernel/broker fact establishes it. Full mediation
requires controlling every permitted exit path, including files, shared memory,
Unix sockets and descriptor transfer, not just TCP ports. This is a data-path and
grant design, not log scraping.

## 6. Falco/Sysdig and Tetragon: useful designs, precise limits

### Falco and the Sysdig-derived capture stack

**Project-documented, source repository inspected:** `libscap` reads driver ring
events and collects OS state; `libsinsp` enriches events into process/file state,
filters them and evaluates rules. These components are shared infrastructure for
Falco and related consumers. That separation is a useful model for Shards'
kernel-fact collector, state reducer, policy/detection evaluator and exporter.
[Falco libs README at `0815626f9fe816aca7fa41cbb776c68770970e16`](https://github.com/falcosecurity/libs/blob/0815626f9fe816aca7fa41cbb776c68770970e16/README.md).

**Project-documented:** Falco's default modern eBPF probe emits events to userspace;
it needs BPF ring-buffer support and exposed BTF. The alternative kernel module
requires full privileges. Embedded bytecode/CO-RE avoids a per-host probe build; it
does not eliminate kernel feature or privilege requirements.
[Falco kernel event sources](https://falco.org/docs/concepts/event-sources/kernel/).
The supported event table distinguishes syscall, tracepoint, plugin and metaevents;
only a subset is enabled by default for performance. It does not promise arbitrary
application tool semantics or full payload capture.
[Falco supported events](https://falco.org/docs/reference/rules/supported-events/).

**Source inspected:** the pinned modern probe uses `sched_process_exec` for
successful `execve` and `execveat`, and syscall-exit handlers for failures; its
source explains architecture-specific reasons for that split. Thus successful
execution and attempted-but-denied execution need distinct coverage checks.
[Exec tracepoint source](https://github.com/falcosecurity/libs/blob/0815626f9fe816aca7fa41cbb776c68770970e16/driver/modern_bpf/programs/attached/events/sched_process_exec.bpf.c#L41-L65),
[failure handler](https://github.com/falcosecurity/libs/blob/0815626f9fe816aca7fa41cbb776c68770970e16/driver/modern_bpf/programs/tail_called/events/syscall_dispatched_events/execve.bpf.c#L9-L39).
The fixed-event reservation helper increments a drop counter and returns without
collecting when ring reservation fails; it does not block the actor until a
consumer catches up.
[Ring reservation source](https://github.com/falcosecurity/libs/blob/0815626f9fe816aca7fa41cbb776c68770970e16/driver/modern_bpf/helpers/store/ringbuf_store_params.h#L89-L119).

**Project-documented:** buffer-full events can be lost, impairing Falco's reconstructed
state and dependent rules. It exposes counts and can log, alert or exit. A userspace
reaction to that loss cannot undo actions already performed.
[Falco dropped-event behavior](https://falco.org/docs/concepts/event-sources/kernel/dropped-events/).
**Recommendation:** borrow explicit loss accounting, typed event schemas and the
capture/enrichment split. Do not mistake detection plus a response program for
pre-action authorization or replace the native domain boundary with a daemon that
discovers descendants after the fact. No independent overhead result is established
here, and this research does not require installing Falco/Sysdig in Shards.

### Tetragon and kernel-local enforcement

**Project-documented:** Tetragon distinguishes overriding a supported kernel
function's return from signaling an actor. Its documentation explicitly warns that
`SIGKILL` during `write()` does not guarantee the write did not occur. Correct
prevention needs the appropriate denial/override, not just a synchronous kill.
[Tetragon enforcement](https://tetragon.io/docs/concepts/enforcement/).

**Project-documented:** kprobe override relies on the error-injection framework and
`CONFIG_BPF_KPROBE_OVERRIDE`; eligible functions are restricted. Uprobe override has
binary/function-signature assumptions and can crash the application. Neither is a
generic, portable interpretation of every tool call in an arbitrary harness.
[Tetragon selector/action contracts](https://tetragon.io/docs/concepts/tracing-policy/selectors/).
**Kernel-documented:** BPF LSM can implement MAC and audit at LSM hooks, including
returning denial from the documented hook example.
[Kernel BPF LSM documentation](https://docs.kernel.org/bpf/prog_lsm.html).

**Recommendation:** where existing kernel controls cannot express a required
security property, evaluate a small kernel-local LSM policy at the right object
hook, separately from event export. Prove coverage and denial timing for the exact
operation; do not infer it from the existence of a tracepoint. Keep policy-map
updates private and atomic with respect to run generations. Make required policy
attachment a launch invariant, and prevent actors from removing it; an alert after
detachment is not a guarantee of continuous enforcement. Specify trusted-supervisor
failure/teardown before relying on that policy's lifetime. This is a possible future
extension, not a claim that
Tetragon itself or BPF LSM is currently enabled in Shards.

### The current kernel does not justify a drop-in sensor assumption

**Source inspected:** both checked-in 6.18 base configs disable KPROBES and FTRACE;
the common overlay enables BPF JIT/JIT_ALWAYS_ON. JIT support for existing filters
is not proof of tracing, BTF, or BPF LSM support. `build-kernel.sh` merges those
inputs, runs `olddefconfig`, and checks requested fragments.
[x86 base](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/resources/kernel/firecracker-x86_64-6.18.config),
[arm64 base](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/resources/kernel/firecracker-aarch64-6.18.config),
[common overlay](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/resources/kernel/shards.config),
[build script](https://github.com/hyper-light/shards/blob/ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8/scripts/build-kernel.sh).
Linux 6.18's BPF LSM option depends on BPF_EVENTS, BPF_SYSCALL, SECURITY and BPF_JIT;
tracing/error-injection options have their own dependencies.
[BPF Kconfig](https://github.com/torvalds/linux/blob/v6.18/kernel/bpf/Kconfig#L83-L93),
[tracing Kconfig](https://github.com/torvalds/linux/blob/v6.18/kernel/trace/Kconfig#L752-L773).
**Unverified:** the effective configuration of every published/locally running kernel
variant. Inspect that exact artifact before making a feature promise; a future
tracing-enabled build needs an explicit security/size/performance assessment.

## 7. Exhaustion, loss and child-to-parent attacks

**Proposed invariants:**

1. An actor has no access to its supervisor's memory, signals, namespace objects,
   credentials, writable policy, or administrative channels. A permitted data
   channel gives no management operation merely because it reaches a trusted
   process. Child output is never parsed as that process's control protocol.
2. Lifecycle operations use handles owned by the supervisor. PID reuse, cgroup-name
   reuse, old certificates, stale queued messages and concurrent run restart cannot
   retarget a kill, subscriber, grant or accounting charge to a new actor.
3. Each queue, parser, pending request, pipe, stream, connection, descriptor and
   spool consumes an admitted resource budget. Stop reading or refuse further work
   before allocating beyond it. Reserve supervisor capacity so a fork/output/FD
   flood cannot prevent termination or recording the reason for refusal.
4. A blocked subscriber cannot block unrelated actors or the control path. The
   supervisor does not hold a global lock across actor I/O, disk I/O, RPC or drain.
   Backpressure must not deadlock an actor waiting for a tool response while its
   output stream is full. Control, tool completion and bulk output require fair,
   independently bounded service.
5. Buffer sizing and batching may adapt to measured rates, latency, available
   memory and scheduling noise **within admitted budgets**. Security grants never
   expand because an estimator saw high throughput, a queue filled, or a retry
   succeeded. Identity, access and resource authorization are not learned facts.
6. Exporter failure, ENOSPC, collector death, malformed input, truncated records and
   event loss produce explicit typed state. Required policy failure refuses launch
   or ends the domain. Mandatory broker auditing fails before granting the next
   operation if it cannot preserve the required record. No catch-all fallback to
   direct sockets, unconfined exec or silent dropping.

An asynchronous kernel-event loss cannot be made lossless after detection. Live
cgroup/proc reconciliation can repair a current inventory but cannot recover a
short-lived process or overwritten arguments. If the product contract requires a
complete durable pre-action process audit, current connector/ring collection does
not meet it. A synchronous kernel admission/recording design or a restricted
broker-only launch mode would need separate specification and evaluation. Freezing
or killing after a gap contains subsequent work; it does not retroactively record
the missing history. This is a known boundary, not a reason to weaken independent
kernel enforcement.

## 8. Implementation direction and alternatives

**Proposed order:** normalize `agent.json`/`harness.json` into a typed launch spec;
extend `domains::Started` into an owned managed-domain interface; preserve existing
pre-exec enforcement; split stdout/stderr into private framed channels; route them
through the owned buffered data path; add lifecycle/health events; then introduce
typed broker operations where semantic authorization is required. Keep actor
metadata supplementary. Research on content filtering and network relay policy is
separate from the process boundary established here.

| Alternative | Appropriate use | Why not silently make it the default |
| --- | --- | --- |
| Existing kernel confinement + asynchronous native event collection | Compatible arbitrary harnesses, contained descendants, process history with explicit gaps | Cannot claim universal semantic or lossless pre-action auditing |
| Typed tool/launch broker; direct bypass operations denied | Operations requiring exact authorization, result identity and audit admission | Unmodified programs expecting unrestricted native exec/files/network need adaptation; in-process code still exists |
| Broad ptrace / per-syscall notification | Diagnostic or explicitly stronger mediation experiment | Complex races and cancellation; no demonstrated launch/steady-state budget here |
| BPF/LSM enforcement + separate event export | Missing kernel-object policy that a suitable hook can express | New effective kernel requirements, additional privileged guest policy surface, coverage proof required |
| External Falco/Tetragon installation | Independent comparison/oracle in a test environment | Extra installation is outside Shards' one-binary delivery; products do not establish all required guarantees |

No alternative establishes “minimal overhead” without measurement. Prefer avoiding
per-syscall userspace round trips where inherited kernel policy already decides the
operation, and avoid payload copies solely to decorate telemetry. The mandatory
buffered relay still owns transferred bytes; optimizing it cannot hand an actor the
underlying unrestricted connection.

## 9. Attacks, fault injection and measurements to run

The following are proposed checks. They are not results. Use real adversarial native
test programs, not only cooperative shell scripts or harness self-reporting.

| Attack / fault | Required observation |
| --- | --- |
| Deep fork/clone/vfork chains, `setsid`, double fork, parents exiting, native raw syscalls and both guest architectures | All descendants remain constrained and accounted; whole-domain stop leaves no live process; no recursion-depth assumption in authority |
| PID churn, early leader exit, concurrent reaper, run restart and stale subscriber messages | No operation reaches a reused identity; setup/exec failure, signal exit and normal exit remain distinguishable |
| Actor refuses to reap; namespace PID 1 ignores normal signals; actor stops itself | Termination remains possible through kernel authority; zombies/resources are eventually reclaimed and status is explicit |
| Close/dup/replace stdout/stderr, shared descendant pipes, binary/invalid UTF-8/no-newline output, terminal escapes and forged control/event frames | No control injection; exact domain/channel attribution; no invented per-process attribution; bounded memory and honest capture scope |
| Fork, thread, descriptor, socket, queued-message, tmpfs, storage and output floods | Admission/refusal at actual budgets; supervisor and other domains remain controllable; no FD/heap/spool leak or busy loop |
| Attempt `ptrace`, process-memory reads/writes, `pidfd_getfd`, `/proc/.../fd`, namespace/cgroup FD acquisition, `SCM_RIGHTS`, and management-endpoint impersonation | Kernel denial or explicit narrow grant; no path from a child-held capability to supervisor authority |
| Concurrent mutation of argv/path/sockaddr while a selected call is stopped; syscall abandonment and notification target death | No authorization based on bytes later reread by the actor's syscall; no operation against a reused PID; all broker-created resources reclaimed |
| io_uring setup, imported/inherited rings and SQPOLL; alternate syscall ABIs, `execveat`, memfd execution, interpreters/JIT | Policy coverage matches the declared guarantee; unsupported operations refuse instead of bypassing observation |
| Event-ring saturation, ENOBUFS, collector/exporter crash, ENOSPC and audit-storage corruption | Explicit gap/degraded/failed state; enforcement remains active; mandatory broker operation does not proceed without required admission record |
| Backpressured output plus an actor waiting on a tool RPC; hung client; disappearing broker during launch/drain | No global deadlock; control/termination remains responsive; lifecycle failure and partial delivery are recorded accurately |
| Supervisor death, VM process death, restart, save/restore and move | No uncontrolled surviving domain/helper or restored stale capability; new generation and policy/stream ownership established before resumed actor execution |

Mutation-check enforcement tests by disabling the particular guard and proving the
attack actually succeeds/fails differently. Do not accept a test that passes because
the kernel, test harness or namespace accidentally prevented the attack before it
reached the guard being tested. Separate prevention from telemetry assertions: an
alert is not proof that a forbidden write/connect never took effect.

**Measurement method:** compare the same signed build/kernel/artifacts with existing
domain startup, the proposed native management path, and any heavier mediation
candidate. Record host/OS/arch/hypervisor/revision, guest kernel pin, workload,
contention and memory/FD budgets. Cover quiet launches, fork/exec bursts, long-lived
idle harnesses, realistic tool workflows, many concurrent domains, and deliberate
output/network saturation. Report n, p50, p90, p99 and max for launch-to-ready,
fork/exec cost, tool latency, output first-byte/delivery latency and termination;
also CPU time, RSS/PSS, kernel memory, FD count, bytes/copies, queue high-water marks,
spool growth, drops and affected neighbors. Do not hide failed or stalled runs.

Use paired/interleaved comparisons and increase sampling until the measured noise
supports the claimed distinction; derive runtime batching/capacity from live
observations within authorized budgets. A host-specific measurement validates the
method; it does not become a universal production constant. Run both Linux guest
architectures on KVM, Hypervisor.framework and WHP where those host combinations are
supported; compile/lint the complete eight-target host matrix and run the relevant
real-VM suites before claiming parity. No numbers in this research establish that
those gates have passed.
