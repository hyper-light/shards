# Native effect authority: issuer and transport choice

ROOT implementation decision,2026-11-11. This is required P7/P11 work, not an
implemented permission boundary or a passing compatibility/performance result.
FullR01–R16 at main749c3ca06, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

## Source facts and the missing producer

The checked domain's `no_processes` comes from the normalized owner declaration.
Prepared root/scratch and PreparedVolume object/read_only/visibility/declared_at
are actual owner-selected objects. ActorOnly is a restriction, never another
member's grant. Existing peer Hand/Send rights and a checked executable object
do not authorize arbitrary native execution or writes.

The native helper's reviewed bootstrap map is null0–2, execution CONTROL3,
setup-only BINARY4, and readiness GATE5. CONTROL3 belongs to the existing
per-domain server's program executor; it is not an init-issued authority channel.
GATE5 is writer-only in the helper and closes after readiness. Reusing it for
receives could consume another launch participant's release acknowledgement.
These are inspected source facts from domains/native_helper.rs,
native_effects/bootstrap.rs, server.rs and the domain producer, not new live
attack reproductions. The running r146 graph stays frozen until its result.

## Chosen authority and transport

The sole root issuer is the existing trusted init/lifecycle owner, using its
actual protected run/member/Epoch, current lifecycle and checked domain objects.
It owns the admission and commit decision. No new global grant registry,
manager process, policy cache, timer or periodic probe is introduced.

Add one private per-member owner channel to the existing child's prepared FD
map and existing owner's event loop. It is an anonymous CLOEXEC SOCK_SEQPACKET
pair created in trusted preparation. Only the native helper retains its child
end; the current map's next unused slot is6. The fixed slot is a bootstrap
protocol invariant, not a capacity/timing parameter; verify it against the
composed map and document it with the other bindings. CONTROL3 and GATE5 keep
their existing ownership and meaning. The actor and tool executables receive
neither the owner channel nor the policy object.

The first typed owner message installs one member's immutable rights snapshot.
Use a sealed, non-executable memfd for the variable-size snapshot, transferred
on that owner channel with the canonical FD-transfer machinery. The payload,
array lengths, path bytes, descriptors and preparation overlap must be charged
before acquisition. Derive sizes from the checked declaration and actual encoded
representation; no maximum-grants constant, RAM estimate or free-space-as-rights
test. Reject a table that cannot fit its actual admitted resources.

The snapshot is written once, then sealed against writing, growth and shrinkage
and against further seal changes. The helper validates the complete typed
version/length/member/Epoch, required seals and exact expected ancillary objects;
it closes rejected objects and resolves/holds only the selected common-view
roots before HelperReady. Seals prevent mutation; they do not authenticate an
issuer. Decoded bytes, memfd identity, Context DATA, O_PATH, executable mode,
labels, argv, adapter IDs and socket kind cannot independently construct an
authorized capability. The actual private owner endpoint, held child association
and protected issuer are required too.

This owner channel remains live for current-owner admission, per-execution
kernel-scope allocation, association, commit/revocation and terminal ownership.
Integrate it into the existing authoritative lifecycle/reactor. A sealed startup
file alone cannot solve current-owner checks or create trusted cgroup objects.
Match the original captured Epoch and owner; never substitute the latest global
generation. Lost owner, stale state or unsupported protocol gives a typed refusal
or Unknown outcome, not a default grant or replay.

The actual issuer endpoint stays with outer trusted init, which holds the
admission/cgroup objects. A namespace reaper without those objects cannot become
an issuer by its role name. Keep the parent's synchronous launch wait in view:
queue the fixed initial record and its one sealed object before child release,
while preparation still owns both endpoints. The helper can then install its
snapshot and announce readiness without waiting for a parent reactor that has
not started serving this member. Current-state commit/admission runs through the
existing owner's live loop after association. No execution may use a fictitious
initial-current acknowledgement. Represent the initialization/association/ready/
retirement transitions explicitly in the actual producer/consumer types.

The existing per-domain server starts listening before its directory can become
the actor-server mount and before Prepared roots/native helper exist. Preserve
that producer ordering too: listening readiness is not native-effect Session
readiness. Do not make Server::start wait for the later helper and deadlock the
very preparation needed to create it. Attach the canonical native Session only
after its real initialized helper/owner/input/audit inputs exist, through the
existing server event path. Identity-binding context may name the expected scope;
it still does not issue the grant or satisfy missing initialization.

## Projection of existing rights

| Existing owner fact | Broker authority it can support | Required restriction |
| --- | --- | --- |
| Checked image/common root | Read/resolve and checked executable selection within that view | A held object is not a process grant; preserve actual filesystem/mount execute checks. |
| Common volume, read_only | Read within its selected object | Never issue a write capability. |
| Common volume, writable | Broker writes within that selected object, after per-call admission/audit | Do not expose a writable backend FD to the actor. |
| Granted scratch | Same explicitly admitted scratch operations | Temporary paths do not imply unlimited bytes, tasks or authority. |
| ActorOnly object | Only the existing actor-side selected contract | Exclude it from helper/common grants; do not promote visibility. |
| Owner permits processes | Checked native child within the permitted view and ancestor budgets | Owner `processes=none` denies broker-requested native program execution too. |
| Parent's actual issued capabilities | An explicitly requested intersection for an owned delegated execution | No new root grant, widened rights, or authority from a reported subagent ID. |

The process permission gates requested untrusted native programs. It does not
turn required trusted management/helper processes into actor execution grants,
or remove already-authorized peer/file service operations merely because their
implementation uses a confined worker. Those workers still require actual
ownership and resource accounting; they acquire no general native-exec right.

The process permission and concrete executable/workdir/object selection are
separate checks. Native permission/type checks never replace owner authorization.
Projection does not turn every read permission into WRITE/EXECUTE or every
checked file into a tool grant. There are no new manifest fields or implicit
network/device grants in this slice.

## Closing direct effect routes

For the effects requiring semantic admission, the strict actor cannot retain
the corresponding direct writable mounts/FDs or native child-creation route.
Build actor and helper restrictions from the same checked owner objects: actor
read-only access where needed, helper-held admitted writes, explicit FD allowlists
and an inherited kernel restriction on uncontrolled process creation while
preserving allowed threads. Do not authorize a syscall to continue after checking
mutable actor pointers. Use the existing trusted native executor.

This requires the real pinned Codex/Claude operation routes, including native
built-ins/fallbacks, to work through that executor. Do not replace implementation
with blanket denial and call compatibility complete. An unsupported required
operation remains an implementation gap; before release its typed refusal cannot
silently fall back to a direct effect. Explicit actor-only credential/state rights
retain their documented mode and are not relabelled broker-owned tool writes.

Already-open writable descriptors, shared mappings, received sockets and alternate
file/process APIs require explicit handling. Landlock does not revoke earlier
open-file rights. The full network/Unix/socket-transfer ledger remains P6; a
native file/process table is not proof that those routes are mediated.

## Actual kernel ownership of executions

Use the existing trusted owner's admission/lifetime machinery to create a held
kernel execution scope for each actual native job. Birth belongs to that scope
before its first untrusted instruction; preserve the existing clone3/pidfd/ACK
path. A pidfd identifies a leader, not its arbitrary descendants.

Keep the owner/reaper outside the untrusted job scope. The held cgroup objects,
kill/events controls and trusted reaping establish population ownership; actors
receive no migration/control handles. Observe actual emptiness and reaping before
returning charges. Killing a leader, EOF or a stop request alone is insufficient.
Unknown native outcomes retain ownership/charges and quarantine semantics.

Logical delegation is iterative and attenuates the ancestor grant/budget. It does
not add a cgroup/PID namespace/Landlock layer per reported depth. Real job leaves
use bounded hierarchy depth under the tenant; account for cgroup-v2 controller
and no-internal-process constraints. Sharing an address space cannot establish
isolated subagent principals merely through tokens or labels.

## Alternatives and primary sources

| Alternative | Decision and source/data boundary |
| --- | --- |
| Extend CONTROL3 to issue its own authority | Reject: the inspected endpoint is the existing server/executor channel, whose current context only grants peer/Hand operations. It cannot bootstrap broader rights from itself. |
| Receive configuration on GATE5 | Reject: the inspected readiness/ACK protocol requires the helper's writer-only use; introducing another reader violates that ownership. |
| Only inherit a sealed table FD | Useful immutable data, insufficient final transport: no live current-owner/per-job scope issuer. |
| Private owner channel plus sealed bounded snapshot | Chosen implementation: one actual issuer, independent bootstrap/control meanings, same existing owner reactor, variable policy transferred as owned data. Resource and performance claims remain to be measured. |
| Observe syscalls or accept reported tool labels | Reject as effect authority: observation and mutable arguments do not supply object grants or owned effects. |

This builds on Shards' existing normalized grants, selected object checks,
prepared FD mapper, native launcher, effect broker and protected lifecycle.
Established object-capability and process-isolation precedent is described by
[Capsicum](https://www.cl.cam.ac.uk/research/security/capsicum/papers/2010usenix-security-capsicum-website.pdf).
Linux6.18's [Landlock contract](https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/userspace-api/landlock.rst)
defines inheritance, descriptor-rights and ptrace boundaries; do not assume later
kernel ABI features. The [seccomp contract](https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/userspace-api/seccomp_filter.rst)
explains inherited syscall restrictions and their limits. The
[cgroup-v2 contract](https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/admin-guide/cgroup-v2.rst)
defines population notifications, controller topology and cgroup.kill handling
of concurrent forks. [memfd_create](https://man7.org/linux/man-pages/man2/memfd_create.2.html),
[Linux6.18 memfd flags](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/linux/memfd.h)
and [openat2](https://man7.org/linux/man-pages/man2/openat2.2.html) provide the
specific immutable-object and scoped-resolution mechanisms. Existing checked
object resolution remains authoritative; no generic replacement is selected.

## Implementation and evidence obligations

Native owns the producer projection/codec/bootstrap consumer and prepared-map
delta at its existing seams; parent owns the actual issuer/current-owner channel,
existing lifecycle/reactor integration and per-execution admission. Agree and
freeze the exact shared signature with the existing input/audit/adapter owners;
do not copy active writer files. The actual canonical Session consumes these real
inputs alongside its audit and input ownership, with one parser/dispatcher.
No constructor from Context/table bytes alone is accepted as completion.

Finish the existing r146 batch before changing its source. New scoped implementer
checks cover exact rights projection, ActorOnly/read-only exclusion, malformed
version/length/seals/ancillary data, returned rejected objects, loss/stale-owner
paths and ordinary FD/ready/cleanup behavior. No held hostile-helper/allocator/
cotenant theft campaign is reauthorized by this decision.

ROOT owns the actual VM producer/consumer proof, direct-effect destination
witnesses and mutations, descendant population/control failures, revocation,
restore/move, seven targets, parent/kernel accounting and comparative costs.
No correctness, compatibility, hard-admission, leak-free or4–5x result follows
from this choice or its source inspection. Required literal derivations, explicit
wire/lifecycle transitions and everyR01–R16 account remain part of implementation.


## Selected native seccomp stages

ROOT selected this experimental implementation at receipt12079. It is not an
accepted production boundary or a passing composed-helper/VM result. Native
owns the canonical policy codec/helper builder; the actual init owner produces
and retains the checked policy objects. No separate process manager is added.

Source inspection establishes the constraint: the current helper installs its
fixed code-worker/lifecycle allowlist before later children. It excludes held
native exec and general runtime operations. Adding exec alone cannot support
those programs. Applying the existing domain profile unchanged to the helper
would also refuse trusted clone3. These are new-feature compatibility facts,
not a claimed live legacy defect. Existing GNU native-exec and code-helper tests
validate separate mechanisms; they do not prove the proposed composition.

Linux documents inherited filters and restrictive stacking in its
[seccomp contract](https://docs.kernel.org/userspace-api/seccomp_filter.html).
The selected stages therefore keep the helper's parent filter as the union of
its existing fixed trusted calls and the actual owner-compiled domain program.
The architecture/x32 guard precedes every helper exception. The fallback enters
the unchanged domain BPF, retaining its relative jumps and decisions. The native
child adds the exact original domain filter after actual Group association,
durable intent/current commit and release, immediately before the unchanged
held-object exec leaf. That intersection removes helper-only exceptions from
the untrusted image. Code workers retain their existing stricter filter.

This follows the late application pattern in
[runc1.3.3](https://raw.githubusercontent.com/opencontainers/runc/v1.3.3/libcontainer/standard_init_linux.go)
while preserving Shards' own single-helper ownership and raw-child FD map. The
existing selected profile source allows map/fchdir/held-exec operations; its
actual compiled guest behavior still needs testing. Rejected alternatives are
an arbitrary native-runtime syscall list, disabling helper filtering, and a
separate precreated executor/manager. The first guesses runtime requirements;
the others discard an existing restriction or add lifetime/resource machinery.

Policy flags and program come from the existing checked domain Filter producer,
not actor bytes or CLI identity labels. Transfer a typed policy record and a
sealed non-executable acquired object on the same private owner6 bootstrap path;
queue initialization while preparation owns both endpoints, before clone/release.
Confined readiness requires actual validated policy installation. Required
seals, byte lengths, native instruction representation and flags are checked;
missing/malformed/stale policy refuses readiness. The fixed descriptor derives
from the reviewed bootstrap map; it is not a capacity default. All lengths and
instruction-count checks derive from the actual program and the
[pinned kernel UAPI](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/linux/filter.h)
and [classic-BPF contract](https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/networking/filter.rst).
Actual encoded bytes, pages, descriptors and preparation overlap remain charged
objects; no hard backing or low-overhead claim follows from their representation.

One explicit NotBorn message is also required for a published Process Group
cancelled before association. Only the trusted helper may report it after
retiring the same pre-birth Worker and every birth/Group capability, so a late
Commit cannot launch it. The actual owner combines that fact with kernel empty
before removal. Cancellation, missing pidfd and EOF never substitute for this
fact. Possible birth instead requires matching trusted reap and kernel empty;
unknown outcomes retain ownership. This closes a new protocol requirement,
without claiming a reproduced legacy leak or completed revocation proof.

For actor protocol input, reuse the selected CONTROL3/owner6 route with a distinct
typed MemberActor message and the existing input/body codecs. MemberActor input
and operation/Worker identities stay separate; there is no synthetic job0 or
reinterpretation of a header width as a tag. The init input Owner keeps the sole
writer/buffer. The actual protected selected launch request and actor association
precede attachment; every write uses fresh original-scope/current WRITE checks.
A decoded Context or requested extent alone grants nothing. A new direct input
endpoint would add dispatcher/descriptor/charge obligations, so the existing
protected route is the implementation candidate. Frame overlap and all payload
ownership must be acquired explicitly; no cross-process mutable borrow or extra
unbounded queue is introduced. Raw input sessions continue to refuse Native.

R01-R16 source/typed transition, actual guest compatibility, meaningful guards,
whole population and parent/kernel/cost gates remain open. These stages preserve
the paused WindowsARM VM and held campaigns. No4-to5x performance result is
claimed; ROOT owns comparable measurements after the actual caller is composed.
