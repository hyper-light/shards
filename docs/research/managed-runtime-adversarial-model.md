# Managed agents: adversarial model and native control boundary

Research baseline: `ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8`, branch
`p/managed-runtime-research`. Sources accessed 2026-11-10 UTC. This is a design
research note, not a claim that the proposed controls have been implemented or
tested. No builds, installations, microVM tests, or performance measurements were
performed for this note.

## The security objective

**Proposed contract:** an `AGENT` or `HARNESS` artifact, its metadata, and every
process it creates are untrusted. This includes well-intentioned software with
bugs, compromised packages, deliberately malicious native code, and cooperating
descendants. Their names, signatures, model providers, claimed tool calls, logs,
and reported measurements do not confer authority.

The runtime must hold a domain's externally effective actions to the owner's
grants, contain its resource consumption, and manage the lifetime of its entire
process population. This must hold when the program never cooperates with
shutdown, replaces its executable, forks continuously, double-forks, changes
session, lies about its identity, writes invalid bytes, or stops reading.

**Proposal, with an essential limit:** enforce effects at kernel-object and broker
boundaries; observe behavior separately. A record of an action is not a mechanism
that prevents it. Native code can make a tool call entirely inside its address
space; an OS observer cannot reliably turn arbitrary instructions or encrypted
application traffic into a trustworthy semantic tool name. Semantic integrations
must identify their authenticated broker or protocol, and self-reported events
must remain explicitly untrusted.

Evidence labels used below:

- **Code observation:** read at the baseline above; not an exploit reproduction or
  a verification of every configuration.
- **Documented fact:** directly supported by an opened primary source.
- **Inference:** a consequence or risk to test, rather than a demonstrated failure.
- **Proposal:** a requirement or implementation direction, not current behavior.
- **Vendor claim:** the vendor's description; proprietary implementation and
  effectiveness have not been independently verified here.

## Existing Shards mechanisms to preserve

The foundation already exists. `domains.rs` starts a domain in its own mount,
PID, IPC, network, UTS, and cgroup namespaces and creates it directly in its
cgroup. It uses a distinct UID/GID, drops capabilities, sets `no_new_privs`,
installs Landlock and seccomp, and gives it an immutable view of the built image,
private scratch, and granted volumes. These are code observations, not a proposal
to install a second runtime. See [domain contract](../../crates/init/src/domains.rs),
[domain creation](../../crates/init/src/domains.rs), and
[restriction before exec](../../crates/init/src/domains.rs).

The wider architecture matters as much as the launcher:

| Existing seam | Security relevance and remaining obligation |
| --- | --- |
| [D59](../design/architecture.md), `init::domains` | Domain namespaces, filesystem view, grants and resource handling. Preserve confinement for descendants without requiring their consent. |
| [D115](../design/architecture.md) | The main workload is a sibling of agent domains, not their privileged parent namespace. Runtime overrides are checked beside domains. A hostile main workload belongs in the threat model too. |
| [D109](../design/architecture.md), [volume opening](../../crates/shards/src/volumes.rs) | Build-time declarations and runtime grants both matter; runtime metadata must not manufacture a volume or host-path grant. |
| [D60](../design/architecture.md), [D139](../design/architecture.md) | Per-domain in-VM servers and explicitly mediated file transfer provide a place for typed tool/resource requests. Avoid moving rich protocol parsing into privileged init. |
| [D123](../design/architecture.md), [process events](../../crates/init/src/procevents.rs) | Kernel-derived fork/exec/exit records are explicitly a record, not a boundary. Loss and unavailable data are represented; threads are omitted. They do not prove every syscall or semantic tool call was captured. |
| [domain seccomp policy](../../crates/shards/src/setup.rs) | The no-capability profile limits socket families and excludes io_uring, keyring calls, userfaultfd, BPF and perf interfaces as described there. A new interceptor must not silently broaden it. |
| [scoped path operations](../../crates/init/src/inroot.rs) | Use opened directory/object authority rather than authorize a string and later resolve a different object. Any new broker needs its own race tests. |

**Current output is insufficient for the new requirement.** The baseline duplicates
one pipe onto both fd 1 and fd 2, then prefixes domain lines into the run's stderr.
This loses the channel distinction; an inherited pipe identifies the domain but
does not authenticate which descendant wrote each byte. It is not yet a private,
port-routed structured output service. See [stdio setup](../../crates/init/src/domains.rs),
[domain output polling](../../crates/init/src/run.rs), and
[line relay](../../crates/init/src/run.rs).

## Assets, principals and trusted computing base

**Proposal:** assign authority to objects, not human-readable process names.

| Principal / asset | Trust and authority |
| --- | --- |
| Human owner and authenticated control client | May issue grants and hard resource budgets. Credentials are scoped explicitly; loading a harness does not automatically grant the owner's whole home or credential store. |
| Host Shards daemon, platform confinement, VMM and helpers | Trusted for the host/guest boundary. The host remains an ordinary user's process. Existing platform isolation is required independently of guest controls. |
| Guest kernel and privileged init | Trusted for enforcement among processes in one guest. An exploited guest kernel invalidates intra-VM isolation; the separate host/VMM boundary must still contain it. |
| Per-domain broker/server | Trusted only for its scoped operations. It must possess fewer rights than global init and must validate every caller and message itself. |
| Agent/harness, descendants and main workload | Untrusted. No control handles, policy mutation paths, raw device access, or other principals' grants by default. |
| Managed relay and audit store | Integrity-sensitive and resource-limited. Untrusted bytes must not become executable policy, terminal control, HTML, log fields, or allocations without validation. |
| Artifact and manifest | Untrusted input whose content identity may be verified. An `entrypoint` selects code; it does not enlarge permissions. |
| Owner files, credentials and secrets | Read and write are separate grants. A legitimate write grant permits destructive changes within its scope; containment cannot undo authorized deletion or secret use. |
| Availability of other domains, workload and supervisor | An asset to protect separately from the attacking domain's availability. A resource kill must have a recorded cause and target. |

**Proposed trust boundaries:**

```text
owner's authenticated control channel
           |
host policy + VMM confinement (existing platform modules)
           |
Linux guest kernel + init: domain membership / rights / accounting
           |
    typed per-domain control and output broker
           |
untrusted harness --> arbitrary descendants and in-process code
           |
 only granted filesystem objects, managed communication, scoped tools
```

The guest controls can be shared across all eight host targets because their
execution environment is Linux; the host adapters remain different. This is an
architectural proposal using Shards' existing guest design, not a promise that
Linux host tracing hooks work on macOS or Windows. Do not require a host EDR,
root daemon, system extension, container stack, or a second installed binary.

## Authority and ownership rules

The following are proposed invariants, with implementation choices to be checked
against existing behavior and compatibility:

1. **Creation is admission.** Validate the complete typed launch plan, reserve its
   control/stdio/accounting resources, establish restrictions, then allow the
   first untrusted instruction. Failures before admission leave no runnable child
   and no leaked rights.
2. **Ownership is a domain lifetime, not a PPID chain.** Keep a private control
   handle for the domain and an authoritative cgroup/namespace identity. Ordinary
   fork, exec, daemonization and session changes must not escape it. The domain
   cannot migrate itself or children into a less restricted cgroup or namespace.
3. **Parent control is never inherited by the child.** Pass only enumerated
   descriptors, with explicit roles and close-on-exec policy. Stdio write ends
   cannot imply access to output readers, policy channels, namespace/cgroup FDs,
   the host connector, credential stores, or other domains' sockets.
4. **Policy is enforced where the effect happens.** Broker operations use copied,
   validated request data and object handles. A child-side check or an event
   arriving later is insufficient. The broker rechecks authorization even when
   the client prevalidates it.
5. **Stop revokes admission first.** Refuse new tools/flows, close or drain admitted
   work under explicit policy, end the complete domain, reap and reconcile it,
   then release resources. Do not declare it gone just because its initial PID
   exited or its main stdout closed.
6. **Revocation has an explicit scope.** Removing a grant can prevent future broker
   operations. It cannot retract bytes already read, secrets already disclosed,
   or necessarily an FD already handed to native code. Rights needing immediate
   revocation should remain behind a broker rather than be delegated as raw FDs;
   otherwise terminate the affected domain and state the limitation.

Linux PID namespace init termination kills its namespace population; cgroup kill
also addresses population lifetime. Neither makes the parent-child relationship
itself a general permission system. For specific-process management, Linux
pidfds support polling, signaling and waiting without treating a later reuse of
the numeric PID as the same object. Establish them without an intervening reap
race. [PID namespaces](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html),
[pidfd API](https://man7.org/linux/man-pages/man2/pidfd_open.2.html).

`no_new_privs` is inherited and blocks exec-based privilege gains, but does not
block all privilege transfers: the kernel specifically identifies received
`SCM_RIGHTS` descriptors as a separate route. Consequently, a successful UID/cap
drop is not evidence that an arbitrary IPC handoff is safe. [Linux 6.18 no_new_privs](https://docs.kernel.org/6.18/userspace-api/no_new_privs.html).

## Capability and attack matrix

Every row below is a **proposed threat and acceptance criterion**, not an assertion
that an exploit was demonstrated or that all listed controls currently exist.
The source column identifies the current seam or a primary explanation of the
mechanism; the final audit must confirm actual enforcement on the pinned kernel.

| Surface / hostile action | Required boundary | Evidence or implementation seam |
| --- | --- | --- |
| Fork/clone storm, threads, double fork, setsid, exec replacement, zombie accumulation | Descendants stay in the admitted domain; resource admission includes threads; no escape by PPID/session changes; reaping is supervisor work. | `domains::start`; [pids controller](https://docs.kernel.org/6.18/admin-guide/cgroup-v2.html#pid). |
| Kill, ptrace, process_vm, pidfd_getfd against supervisor or sibling | Parent/sibling isolation must hold even where ordinary debugging within the domain is supported. Test namespace, UID, Landlock and capability composition rather than claim seccomp denies every tracing syscall. | [D115](../design/architecture.md), [actual default profile](../../crates/seccomp/data/default.json). |
| setuid/setgid, file capabilities, ambient caps, unshare/setns, privileged clone flags | No elevation or membership escape; no inherited handles that bypass the policy. Retain restrictions across exec. | [domain setup](../../crates/init/src/domains.rs); no_new_privs source above. |
| Enumerate /proc, /sys, cgroupfs, kernel logs; change resource controls | Only intended namespace view and no writable management objects. Reject runtime flags that expose ancestor namespaces or dangerous privileges. | `domains.rs`; D115. |
| Inherit/receive raw FD, SCM_RIGHTS, socketpair; duplicate a privileged handle | Explicit descriptor allowlist, peer authentication and rights attenuation; no direct transfer of a socket whose traffic must remain mediated. Test fd reuse and cancellation. | [Landlock FD semantics](https://docs.kernel.org/6.18/userspace-api/landlock.html#rights-associated-with-file-descriptors). |
| Symlink/magic-link/hard-link/rename/mount races; absolute or non-UTF-8 paths | Resolve relative to held roots, with intended traversal semantics; operate on the resulting object. No check-then-open through a mutable global path. | `init::inroot`; [openat2 resolution](https://man7.org/linux/man-pages/man2/openat2.2.html). |
| Modify built artifacts, sibling source, grants or manifests after checking | Immutable as-built metadata; verify launch inputs and grants at runtime too. A malicious image cannot grant itself paths/devices or hide unsafe overrides. | D59, D109, D115; `build::domains`. |
| Device nodes, dangerous ioctl, raw memory, guest/vsock devices | Minimal device namespace and narrowly granted device rights; never hand the domain the supervisor's tty or control device. | `domains::DEV`; [Landlock ioctl limits](https://docs.kernel.org/6.18/userspace-api/landlock.html#ioctl-support). |
| mmap/mprotect, JIT, shared memfd, executable writable files | Preserve ordinary language-runtime compatibility where permitted, but keep mappings of trusted control memory and executable artifacts inaccessible. A memory-safe broker does not make shared writable input immutable. | `domains` filesystem/FD boundary; requires dedicated race tests. |
| io_uring, BPF, perf, keyrings, userfaultfd, unknown syscall/compat ABI | Do not bypass mediation through another API. Retain the current reduced syscall surface; explicitly review any compatibility exception and architecture variant. | [domain_profile](../../crates/shards/src/setup.rs), [seccomp limits](https://docs.kernel.org/6.18/userspace-api/seccomp_filter.html). |
| Raw network, AF_VSOCK, alternate socket family, direct DNS, loopback, IPv6, UDP/QUIC | Every reachable communication route is covered by the same grant/relay plan. A proxy environment variable is compatibility configuration, not enforcement. | Domain socket-family policy; `netplan`; separate buffered-relay note. |
| Unix path/abstract socket, POSIX/SysV IPC, signals, shared-memory rendezvous | Scope or deny cross-domain routes; protect broker endpoints from impersonation and credential/FD laundering. Allowed IPC is still an authority transfer. | `domains` IPC namespaces, grants and Landlock scope. |
| Forge tool names, replay request IDs, make a tool a confused deputy | Authenticate the domain from its channel, enforce a typed operation and its resource grant, bind replies/cancellation to that request generation. Never accept authority from text inside a log. | D60/D139 per-domain server. |
| Huge/deep/malformed frame, truncated JSON, invalid UTF-8, integer overflow, regex/parser blowup | Checked lengths and arithmetic before allocation, fallible growth, bounded work charged to the actor, typed protocol versions and errors; no production panic or permissive fallback. | Existing ABI/frame/parser seams; validation plan below. |
| Slow reader, endless partial frame, output without newline, connection churn | Per-actor retained-byte and in-flight-work accounting, independent scheduling and cancellation; one stalled output cannot block global supervision. | Current D59 line-bound fix; relay design must extend it. |
| Close/replace fd 1 or 2, reopen a file as output, log through a socket or shared memory | Automatic startup capture is required but is not proof of all future output capture. Define and mediate every allowed external effect; a chosen FD number is not a permanent output identity. | Proposed FD/effect policy; existing `domains` launch redirection alone is insufficient evidence. |
| Change policy while a request is pending, PID/FD/port reuse, reconnect/restore replay | Generation-aware identities and an explicit admission/revocation point; no stale token authorizes a new object. Restore re-establishes grants rather than trusting an old numeric identifier. | Proposed domain/control handle; D109 runtime checks. |
| Tamper with logs, flood events, spoof host/provider messages, escape terminal/HTML output | Separate control from data; authenticate provenance at the capture point; retain raw bytes safely and render an escaped view; represent loss and truncation explicitly. | `procevents`, framing, host log storage. |

Two documented limits deserve particular attention. Landlock rights attached to
opened FDs can survive passing them to another process; its ioctl restriction
does not retroactively constrain pre-existing descriptors. Its 6.18 network
rights describe TCP port actions, not a complete arbitrary-protocol firewall.
Some filesystem metadata operations are also outside its handled rights. Keep
mount/UID/network constraints and descriptor hygiene as independent layers.
[Linux 6.18 Landlock userspace guide](https://docs.kernel.org/6.18/userspace-api/landlock.html).

An inspect-and-continue syscall interceptor is especially dangerous. The Linux
6.18 UAPI warns that `SECCOMP_USER_NOTIF_FLAG_CONTINUE` can race with rewrites of
pointer arguments. A policy decision on copied bytes does not cause the kernel
to use those bytes if it later resumes the original syscall. **Proposal:** have
the broker perform the permitted operation itself on held objects, or keep an
independent kernel restriction that remains safe after argument substitution.
Do not sell user notification as a universal security monitor. [Linux v6.18 seccomp UAPI](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/linux/seccomp.h).

## Exhaustion must be contained as carefully as escape

**Documented foundation, not a complete policy:** cgroup v2 distinguishes a
throttling/reclaim threshold (`memory.high`) from `memory.max`; the former is not
a hard memory ceiling. PID limits count kernel tasks/threads and can reject new
fork/clone. `cgroup.kill` handles concurrent forks while killing the subtree.
Memory charges do not simply follow a process when it moves. These facts support
hierarchical ownership, but do not specify Shards' admission policy or cover
every resource by themselves. [Linux 6.18 cgroup v2](https://docs.kernel.org/6.18/admin-guide/cgroup-v2.html).

The following are **proposed accounting obligations**. All ceilings must come
from owner authority, the available host/guest resource envelope, protocol
representation, or a documented derivation. Performance estimators may tune
within that envelope.

| Resource | Attacks and required accounting |
| --- | --- |
| CPU and scheduler work | Endless computation, busy polling, fork/exec churn, parser/crypto cost and excessive timers. Charge child work and broker work done on its behalf; reserve supervisor progress and enforce domain entitlement. Protect other domains, not only host CPU. |
| RAM, kernel/slab memory, page tables and pinned pages | mmap/anonymous memory, many small objects, huge virtual maps, socket queues, pipe growth, mlock and pinned I/O buffers. Account the whole cgroup and attributable broker allocations, not RSS alone; deny allocation before it consumes the supervisor's reserve. |
| Processes/threads | Count all tasks, including thread storms and short-lived children; bound pending starts and zombie/reap work. `--processes=none` must not imply an unlimited thread budget. |
| FDs and sockets | Per-process and aggregate domain use, pending accepts, duplicated/transferred FDs, pipes, epoll/eventfd/inotify/pidfds and queued rights. Keep enough trusted capacity to refuse and cancel under exhaustion. |
| Disk bytes and inodes | Tiny-file/inode storms, sparse files, hard links, xattrs, deleted-but-open files, writable owner shares and append-only audit growth. Limits on RAM or a virtual disk's apparent size are not a substitute for write-volume and inode policy. |
| tmpfs, shmem and dirty pages | `/tmp`, `/dev/shm`, shared mappings, page cache, writeback and kernel metadata. Memory-backed scratch still consumes finite resources; shared-file charge attribution needs explicit tests. |
| Output and telemetry | Endless non-newline output, many tiny events, unread streams, huge events, disconnected audit client, compression expansion and replay backlogs. Bound bytes and event/record overhead independently. Supervisor control and stop remain serviceable. |
| Network state | Many quiet flows, UDP destinations, DNS requests, SYN/accept queues, retransmission buffers, per-peer stream IDs and TLS handshake work. Per-flow caps alone do not bound the sum; admission must charge the domain and relevant shared service. |
| Broker/tool requests | Deep queues, concurrent tool calls, long-running tools, cancel races and deliberately slow remote endpoints. Admission consumes a scoped credit; completion/cancellation returns it exactly once. |

`RLIMIT_NOFILE` is per-process and also constrains unprivileged in-flight
SCM_RIGHTS FDs; descendants can multiply per-process allowances, so it is not
an aggregate domain FD budget. Other limits have distinct UID/process semantics.
Do not substitute a familiar `ulimit` for population accounting.
[Linux resource limits](https://man7.org/linux/man-pages/man2/getrlimit.2.html).

Tmpfs has separate byte and inode sizing controls; xattrs also consume its inode
space. It can swap if allowed. Therefore scratch policy must account for both
memory pressure and object counts, and verify the actual kernel/mount options
rather than assume tmpfs is free or equivalent to a persistent disk quota.
[Linux 6.18 tmpfs](https://docs.kernel.org/6.18/filesystems/tmpfs.html).

### Two current policy seams needing adversarial validation

**Code observation:** [Memory::relieve](../../crates/init/src/domains.rs)
checks the aggregate domains' high threshold and chooses the populated domain
with the largest `memory.current`. This deliberately implements
[D59 part nine](../design/architecture.md), where the owner previously chose
aggregate protection and optional per-agent asks instead of imposed per-agent
shares.

**Inference, not a reproduced exploit:** a smaller attacker could push total
use over the threshold while a larger legitimate domain is selected as victim.
The test is a legitimate memory holder plus a smaller attacker that repeatedly
provokes pressure. If protection of every sibling is now required, entitlement
or admission isolation is a policy change to make explicitly; a better estimator
alone cannot resolve whose resource claim is authoritative.

**Code observation:** the domains parent enables `+pids +memory`, while individual
`pids.max` and `memory.max` are set only when the corresponding optional asks
exist. The higher run hierarchy enables additional controllers elsewhere.
Consequently, this domain seam by itself does not establish per-domain CPU/IO
fairness or a mandatory individual thread allowance. These are audit/test items,
not a claim that host/run resource containment is absent.
[domain controller setup](../../crates/init/src/domains.rs),
[optional limits](../../crates/init/src/domains.rs),
[run hierarchy](../../crates/init/src/run.rs).

### Learning is not permission

**Proposal:** model an immutable or owner-revisable `Authority` separately from
`ObservedCost` and a bounded `OperatingPlan`. Learning bandwidth, RTT, CPU cost,
buffer occupancy, or pressure may change scheduling, chunk size and concurrency
only within admitted authority and budgets. An actor cannot obtain additional
network destinations, filesystem rights, tool privileges, memory entitlement or
priority by inflating a measured workload, forging telemetry, or retraining a
normal-behavior model.

Derive buffer capacity from the granted retained-byte budget and actual protocol
overhead; derive connection/work counts from the available counted resources.
Read OS facts from trusted interfaces. Self-reported actor statistics are
advisory. When learning is absent, corrupted, stale, or adversarially noisy, use
the same safe authority envelope and state the performance uncertainty. No
unmeasured safety multiplier or chosen timeout is introduced by this note.

## Lessons from primary implementations and research

### Chromium: a broker must possess and recheck a narrow policy

**Source verified:** Chromium's Linux syscall broker exposes specific operations
and performs its file permission checks in the broker host before operations
such as open. Broker process setup owns its IPC endpoints and its teardown kills
and reaps its broker child. This provides a useful comparison for a typed Shards
broker: client validation is an optimization; the privileged side is the
authority. It is not evidence that string-based checks in another program are
race-free. Chromium's assertion/abort style is not permission to introduce
production panics in Shards.
[Broker host implementation](https://raw.githubusercontent.com/chromium/chromium/main/sandbox/linux/syscall_broker/broker_host.cc),
[pinned broker process implementation](https://chromium.googlesource.com/chromium/src/+/c67d9954482723e16d0abc8ca2a648b1c44207d5/sandbox/linux/syscall_broker/broker_process.cc).

### gVisor: mediation can reduce exposure, with explicit cost and coverage

**Documented facts:** gVisor implements application syscalls in its Sentry rather
than passing them through as host syscalls, minimizes the Sentry's host surface,
and relies on external resource and network policy for DoS and network control.
It does not promise protection against all hardware side channels. Its ptrace
explanation distinguishes emulation from racy authorize-and-resume inspection.
These are useful boundaries for Shards to state honestly.
[gVisor security model](https://gvisor.dev/docs/architecture_guide/security/).

The systrap platform uses seccomp traps and replaced ptrace as the default; the
platform guide describes different context-switch and deployment tradeoffs.
Netstack owns protocol processing, while host networking deliberately trades
isolation for native networking. A routing option therefore changes the security
model, not just the performance setting.
[Platform guide](https://gvisor.dev/docs/architecture_guide/platforms/),
[networking guide](https://gvisor.dev/docs/architecture_guide/networking/).

gVisor's resource model includes Sentry overhead and application memory, which
may appear as shmem rather than anonymous RSS. Its performance guide separates
structural mediation costs from implementation costs and includes historical
benchmark conditions. None of those figures measures Shards or establishes
negligible interception overhead here.
[Resource model](https://gvisor.dev/docs/architecture_guide/resources/),
[performance methodology](https://gvisor.dev/docs/architecture_guide/performance/).

**Inference/proposal:** retain Shards' existing Linux guest and small enforcement
surface. Adopting a second general Linux syscall implementation would add a
large compatibility and TCB obligation. Borrow the separation of authority,
accounting and mediation; do not install gVisor to satisfy this design.

### Capsicum: explicit object authority, scrubbed creation and honest lifetime

**Peer-reviewed source:** Watson et al., USENIX Security 2010, describe
capability-mode processes, attenuated FD rights, inheritance/delegation and
scrubbed creation. The paper distinguishes object capabilities from Unix
privilege bits. Its process descriptors support lifetime management but reference
cycles expose a logical-application lifetime limitation. Its directory discussion
also shows why colluding renames make apparently local containment checks subtle.
[Capsicum paper, sections 2–3](https://www.cl.cam.ac.uk/research/security/capsicum/papers/2010usenix-security-capsicum-website.pdf).

**Proposal:** use these principles without pretending Linux FDs acquire FreeBSD
Capsicum rights masks. A Shards grant should name an object, operation, delegate,
lifetime and budget. Delegation only narrows it. Keep control handles out of the
managed process, avoid ownership cycles, and document revocation of already
delegated FDs separately from refusal of future requests. Historical Capsicum
performance is not a sizing source for Shards.

### WASI/Wasmtime: explicit imports do not remove accounting obligations

**Documented example, not a native-harness dependency:** the accessed Wasmtime
`WasiCtxBuilder` documentation (52.0.0-dev) defaults to no environment,
arguments or preopened directories, closed stdin, discarded output, and denied
socket addresses unless the embedder configures them. It makes imported authority
and stdio explicit; it does not establish one universal policy for every WASI
embedding. [WASI context builder](https://docs.wasmtime.dev/api/wasmtime_wasi/struct.WasiCtxBuilder.html).

Its resource limiter covers selected Wasm resources rather than every runtime or
embedder allocation; CPU control is separate. Wasmtime also requires embedders to
validate guest values. The lesson is to include broker-side cost and parsing in
Shards' threat model. It is not evidence that wrapping native Codex in Wasm would
be compatible or free.
[ResourceLimiter](https://docs.wasmtime.dev/api/wasmtime/trait.ResourceLimiter.html),
[Wasmtime security boundary](https://docs.wasmtime.dev/security-what-is-considered-a-security-vulnerability.html).

### CrowdStrike Falcon: useful operational comparison, opaque implementation

**Vendor architectural claims:** CrowdStrike describes Windows kernel callbacks
for visibility and prevention, user-mode components using protected processes
and platform APIs, and a kernel presence intended to continue operating or
restart a tampered-with user service. It also describes disabling some
functionality on untested kernels. These are useful examples of separate
visibility, tamper-resistance and compatibility concerns, not proof that Falcon
captures every action or adds no overhead. [CrowdStrike's architecture account](https://www.crowdstrike.com/en-us/blog/tech-analysis-kernel-access-security-architecture/).

**Public vendor configuration:** CrowdStrike's Linux `falconctl` documentation
exposes `auto`, `bpf` and `kernel` backends for supported sensor versions. It also
documents maintenance protection. That establishes supported configuration
concepts, not their exact hook coverage, bypass resistance, loss behavior,
plaintext visibility or per-event overhead. Falcon is proprietary; this research
does not infer its internals from its marketing or claim independent assurance
of comprehensive interception.
[CrowdStrike falconctl documentation](https://developer.crowdstrike.com/falcon-sensor/ansible/modules/falconctl/).

**Vendor-reported failure case:** CrowdStrike's Channel File 291 RCA describes a
validator/interpreter input-count mismatch and missing runtime bounds check that
caused Windows crashes, with expanded validation and testing among mitigations.
This is a concrete reason to test policy *content* against its consuming schema
and actual interpreter, not just sign a configuration file or test wildcard
cases. It is CrowdStrike's account, not a new independent incident analysis.
[Channel File 291 RCA, pages 2–5](https://www.crowdstrike.com/wp-content/uploads/2024/08/Channel-File-291-Incident-Root-Cause-Analysis-08.06.2024.pdf).

**Proposal:** apply that lesson to typed versioned Shards control frames, policy
changes and event decoders. Rich regex/content classification should not run in
a kernel hook or privileged global supervisor if a smaller per-domain component
can perform it safely. A telemetry sensor, even with prevention features, is
not a replacement for mandatory domain rights and resource limits. No Falcon
installation or host privilege is needed for this work.

## A staged native implementation within Shards

These are proposed seams, not a parallel runtime or an instruction to change
unrelated code while research is in progress.

| Phase | Existing modules | Concrete resulting behavior and review gate |
| --- | --- | --- |
| 1. Freeze a checked launch contract | `agentfile`, `build::domains`, artifact loading, ABI, `init::domains` | Parse root `agent.json`/`harness.json` into literal argv/options. No shell expansion and no installation through RUN/VOLUME. Manifest requests cannot grant rights. Build and runtime validation share typed rules. |
| 2. Express domain lifetime and delegated authority | `init::domains`, process handle/platform helpers, per-domain server | Private owned domain handle with explicit preparing/running/stopping/ended transitions; atomic admission, exact-once cleanup and whole-domain cancellation. Preserve the existing privilege-dropping path and compatibility. |
| 3. Make output an admitted private resource | `init::run`, frames/ABI, host log store and relay | Separate stdout/stderr capture at spawn; source/channel/sequence metadata added by trusted capture, raw bytes preserved; output defaults to the managed endpoint with explicit failure/backpressure policy. Exact descendant attribution remains separately specified. |
| 4. Close resource and communication bypasses | `domains`, `netplan`, network process, volume and server code | Per-domain total accounting, reserved supervision capacity, policy-enforced mediated paths for permitted traffic and tools, no raw socket/FD bypass around mandatory buffering. Cross-domain fairness is a policy decision made explicitly. |
| 5. Add trustworthy semantic integrations | D60/D139 server, native protocol adapters, process records | Brokered tool operations have authenticated provenance. Harness-native logs are retained as self-reports. Process lineage and connection metadata correlate evidence without claiming to read encrypted semantics. |
| 6. Harden and measure the final path | Existing real-VM E2E, fuzz/oracle tools and eight-target lint | Attack-driven tests, fault injection and comparable measurements on actual supported hypervisors/architectures; no security or overhead claim based only on compilation or mocks. |

Keep parsing/validation outside the post-fork child path where practical; the
existing launcher already prepares typed arguments and then performs restricted
kernel setup before exec. Output redirection is owned descriptors and framing,
not `bash -c`, `nohup`, `tee`, a preload library, or required user piping. If a
manifest includes a program named `nohup`, it is just another literal program;
its behavior cannot detach the domain from lifetime control.

**Proposal for communication:** preserve end-to-end TLS and existing authenticated
Shards control channels. Mediation can own, account, schedule and relay ciphertext
without becoming a TLS man-in-the-middle. Payload inspection or semantic tool
classification is available only where Shards owns a protocol endpoint or the
owner explicitly grants termination; it is not implied by permission to relay.
Keep output port selection distinct from outbound service ports and permission.
An automatically allocated endpoint must report its actual address to the owner
and cannot expose itself to other VM processes merely because they know a port.
The separate [buffered-relay research](managed-runtime-buffered-relays.md) covers
the transport decisions and their current status.

## Attack-driven validation and measurement gates

The following is a **proposed test plan**. No pass result or performance number
is asserted. Test the effect the control forbids, not merely its configuration.

1. **Descendant containment and lifecycle:** native attack fixtures fork, clone
   threads, double-fork, setsid, exec rapidly, ignore signals, hold pipes open,
   crash namespace init, exit between identity reads, and race stop with spawn.
   Assert all tasks remain attributed to the domain, no control FD is inherited,
   accepted shutdown empties the population and closes resources, and unaffected
   siblings continue. Include owner/client/relay death and snapshot/restore.
2. **Reachability:** fixtures attempt supervisor/sibling ptrace, signals,
   process memory access, proc-FD access, namespace changes, cgroup migration,
   device ioctl, BPF/perf/io_uring, every supported socket family and raw/vsock
   bypass. Exercise allowed ordinary language runtimes and debugging separately
   so a false-deny is visible alongside denied attacks.
3. **Filesystem races:** symlink/magic-link/rename and hard-link races; mounted
   path replacement; raw-byte paths; directory permission changes; grant revoke
   during use; inherited/open/transferred FD cases; immutable artifact and
   configuration mutation. Check host files and sibling data remain unchanged.
4. **Protocol fuzzing:** manifest, request, frame, event and reply decoders get
   truncation at every boundary, bad versions/flags, oversized counts, wraparound,
   duplicate IDs, replay, invalid encodings, nesting, and cancellation races.
   Fuzz parser-plus-authorizer-plus-effect together, not just syntactic parsing.
   Failure is typed and scoped; production code must not panic or leak.
5. **Exhaustion:** attacks independently fill memory/shmem/kernel objects,
   processes/threads, FDs, inode space, retained logs/events, socket queues, DNS
   state and broker work. Add slow/no consumer and no-newline output, then mix
   attacks. Track supervisor heartbeat/stop progress and a sibling's useful work;
   VM survival alone is too weak. Reproduce the largest-domain victim hypothesis
   before proposing a change to D59's current policy.
6. **Mandatory mediation properties:** every admitted effect has one authority
   decision and attributable budget; cancellation returns every reservation
   exactly once; rights of a delegate are a subset of the grant; replay and
   identifier reuse do not change the object; accounting includes broker-held
   bytes; disallowed routes send no bytes. If audit is optional, loss is explicit.
   If an operation requires durable audit, lack of audit capacity prevents its
   admission rather than silently authorizing an unrecorded effect.
7. **Mutation checks:** remove each boundary separately: FD close list, cgroup
   membership/kill, sibling namespace protection, checked length, broker-side
   rights check, aggregate byte accounting, no-bypass route and cancellation
   release. Its corresponding attack test must fail on the mutated build.
   Controls that produce no observable change need stronger tests or a revised
   claim, not assumed credit.
8. **Fairness and failure injection:** exhaust a domain while another writes,
   reads, connects and stops; disconnect audit/relay endpoints; inject ENOMEM,
   EMFILE/ENFILE, short I/O, partial TLS frames, disk-full and stale identity.
   Verify no global logger/broker stall, no fail-open policy, and no cleanup of
   user data outside the owned ephemeral resources.

**Benchmark proposal:** compare the same pinned Shards/kernel/artifact revision
with each added control enabled and disabled in paired/interleaved runs, while
preserving the same security workload and grants. Report `n`, p50/p90/p99/max,
raw samples, machine/OS/hypervisor/architecture, CPU load, available memory,
noise and error/loss counts. Measure launch, fork/exec, tool round-trip,
stdout/stderr throughput and delivery latency, TLS relay goodput and tail
latency, idle CPU, retained bytes, per-flow/request memory, stop/drain latency,
and sibling progress under attack. Separate cold/warm paths and blocked-client
conditions. Determine repetition from observed variation and record it; no
chosen sample count or promised zero-overhead figure belongs in the design.

Use real guests on KVM, Hypervisor.framework and WHP, including both guest
architectures and the Windows arm64 path. Compile/lint all eight host targets;
glibc and musl have separate compatibility obligations. Report unsupported
kernel rights and conditional coverage, not a synthetic all-green statement.

## Explicit limits and unanswered design decisions

- The guest kernel, its enabled LSM/namespace features, VMM, host kernel and
  hypervisor remain part of the relevant trusted computing base. This proposal
  does not establish resistance to unknown kernel/hypervisor bugs or all
  hardware side channels.
- Mandatory effect control, exhaustive syscall observation, complete byte
  capture, and trustworthy semantic tool telemetry are different requirements.
  The current process connector meets none of the latter three universally.
  Specify which record is required and what happens when it cannot be retained.
- A delegated writable volume can be damaged within its granted rights; an
  allowed secret can be used by its holder. Scope and revocation policy are
  necessary, but cannot erase past disclosure or authorized writes.
- Exact attribution of bytes from descendants sharing an inherited output FD
  remains a separate mechanism/design decision. Do not invent a PID from text
  inside the stream or market domain-level capture as exact writer attribution.
  A native program can also close or replace its own stdio descriptors. Initial
  pipe redirection therefore does not establish interception of every later
  file/socket write; that stronger contract needs a specified effect policy and
  independent coverage tests.
- Strong sibling availability guarantees may conflict with D59's deliberate
  aggregate memory victim selection. This needs an explicit owner-facing policy
  decision after reproduction, not a silent implementation change.
- The final generated kernel configuration and runtime ABI probes, not latest
  upstream documentation alone, determine available enforcement. In particular,
  TCP Landlock rights are not proof of UDP/QUIC mediation.
- Runtime metadata, trace buffers and queues are attacker-driven resources;
  completeness and unbounded retention cannot both be promised. Lossless
  required paths must backpressure or deny admission within finite authority.
- No EDR or sandbox vendor's claims establish Shards' coverage, containment,
  compatibility or latency. Those must be demonstrated on Shards' final path.
