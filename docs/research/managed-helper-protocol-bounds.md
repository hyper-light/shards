# Managed startup/helper protocol bounds

Source derivation checkpoint, 2026-11-11 UTC. This records the frozen parent
lifecycle implementation and the existing control stream; it selects no new
capacity, deadline, topology or production change. The newer SIGNAL/Pending and
semantic-failure caller integrations still need immutable source inputs. The
later frozen Failure leaf is now distinguished below from the r37 schema.
A missing input below is not a reproduced defect.

POLICY_READ: implementer/primary researcher `01a1c5bda0bf766683384c0705c0a9bc`;
canonical main `0c598386ca1f7c65c6e0634dba77983e4abffff2`, full AGENTS.md,
R01–R16/evidence/CLAUDE/managed plan and updated `/Users/adalundhe/g121-handoff.md`
read. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only this new document is writable. No runner, build, actor, simulated fault,
profile, benchmark, broad audit, child agent or production edit belongs to this
task. ROOT owns subsequent execution, integration and main merges.

Continuation receipt: main `b6af4ec8efa246c8a12fb44abd10425a5adc9af5`, same
role/path/rules hash. AGENTS/rules/evidence reread in full; CLAUDE is byte-identical
to the prior full read, the managed plan's sole capture-coverage update and
updated handoff through line696 were read. The inspected existing source bytes
remain unchanged; no branch rebase, fetch, build or actor was performed.

## Exact inspected source boundary

Use the following identities rather than a parent's active working files.
`P` below means the existing read-only evidence directory
`/Users/adalundhe/Projects/shards/.claude/worktrees/managed-parent-admission-kiln/docs/research/measurements/managed-parent-admission`.
Parent signed HEAD `082deba33eb825041348e493edaff4be036586d7` does not contain
the new lifecycle leaves. An old HEAD is not a source identity for their draft.

| Frozen input | Bytes / SHA-256 | What it establishes |
| --- | --- | --- |
| `P/records/pidfd-ack-wait-v1/raw.tar.gz` | 34062 / `a8132c5ef304e77c00797a764726e9da814b96a51ff6620599bf803ec0c12b1a` | r37 source/receipt archive: 18 payloads plus manifest; whole archive hash and the relevant source members checked here, not a new audit of every parent receipt. |
| archive `source/crates/abi/src/run/lifecycle.rs` | 11035 / `7a8d15ba9ede337b7da63aba3cd4e567279c2b70263e90e9504dbfa78270d6c1` | Canonical draft wire codec, called **L** below. |
| archive `source/crates/init/src/managed/control.rs` | 16553 / `c81afb20019079be15dddca62d7af207345c0c510a911b13204f8936041237c0` | Native transport, called **C**. |
| archive `source/crates/init/src/managed/receipts.rs` | 10326 / `8b94be69118e90d2dcfbfc3982d9f9ca22aec5037585187e0125a85914d63e21` | Outer receipt owner, called **R**. |
| archive `source/crates/init/src/managed/setup.rs` | 4702 / `8597f250d32c346c75ec9a245e3720dfa946d80a24e6d09e4cb088d398dfc2da` | Actor setup/gate, called **S**. |
| archive `source/crates/init/src/managed/reaper.rs` | 15384 / `7d8ac41c25bdd0da6b31d37a83ab29871e77fca93f623ee9135598c374376ac5` | Native reaper, called **T**. |
| archive `source/docs/research/measurements/managed-parent-admission/pidfd_transport.rs` | 8884 / `276f5b415d8488ba675c94bd69fb2a4b534adb1f8a8c619a680724bafd5b7df6` | Actual r37 selected mechanism fixture. |
| `P/LIFECYCLE-CHECKPOINT.patch` | 101773 / `712d001691a362e1db55182ca547b736a2daac04b4549b2d7fca79398c86dcec` | Exact managed.rs hunks against Git blob `347aa29ed`; reconstructed in memory, never copied from an active source file. |
| `P/HELPER-GATE-PREPARATION.patch` | 628 / `11d779272f3056082f15040263ac123da42290e22fa33dfb73f0d37760ff2358` | Borrowed gate getter/import delta only. |
| reconstructed `crates/init/src/managed.rs` | 55837 / `8b76aef5ca3d45206b7b67ee854b1e6b91199fae6237b56413eb01abe8e04cc0` | **M** below: both preceding patches, exact hunks verified. Intermediate hash `754553d10d7d33b698f6ff99e303fb9a802b1a2d984baf6d6d7ba35add53cc39`. This getter was not compiled by r37. |

The archive's exact Cargo.lock is SHA-256
`6450828153747a139ebef06c8cd6b80d0f9ead4c386ae1c8780bdc79cf71d343`;
libc is 0.2.189, registry checksum
`3eaf3ede3fee6db1a4c2ee091bf8a8b4dccdc6d17f656fb07896ee72867612f2`.
Native helper FD/view proposals are pinned separately at Git commit
`21f0e2453d01d4a1f47c4357ef77d297169e8297`, documents
`docs/research/measurements/managed-native-effects/{HELPER-ASSOCIATION,HELPER-CONTRACT,IPC-FD-MAP}.md`.
They are proposals/dependency evidence, not a compiled complete caller gate.

Existing sources use main `0c598386ca1f7c65c6e0634dba77983e4abffff2`.
The following four files are byte-identical to task-start ca3:

| Existing file | SHA-256 |
| --- | --- |
| `crates/abi/src/run.rs` | `13e34628f57be1e9faac07454c634a3332fc464edc6f1af7938a7a50155fb8ca` |
| `crates/init/src/run.rs` | `0baea0297b8f00ee7f32ec72cac9ef605c6e1bdbbb14d24b070cbd77035adb23` |
| `crates/init/src/hostbind.rs` | `cc91b586e2610801b13a046526d2ee0e41dd2a8cac759e7be4eb5dfa3e8067ad` |
| `crates/init/src/domains.rs` | `1e61e8215ced9bb9a0c6a34ed3599c119f7a1cb28c4e78a258415fd5f33896c7` |

The framing implementation is `crates/init/src/frames.rs`, SHA-256
`e32e1ff869f6a2dbd0ac4c706e50fd3cb96ff6f65d7187d94566a86d91126ca7`.
Only the relevant constructors, wire routines, waits and callers were inspected.

## Lifecycle bytes, correlation and status

**L**, `PACKET_BYTES`, `Packet::{encode,decode,validate}`: the wire is version u8,
kind u8, PID u32 BE, detail i32 BE, step u8. Thus `1+1+4+4+1=11` bytes.
`Packet` has ordinary Rust layout: 11 is not its in-memory size. Version 1 and
the tags below are explicit protocol assignments, not OS limits or queue depths.

| Kind / tag | PID and detail validation | SCM_RIGHTS count |
| --- | --- | --- |
| ActorBorn / 1 | actual actor PID must be 2; detail 0 | 1 actual actor pidfd |
| BaseReady / 2 | PID 0; detail 0 | 0 |
| HelperBorn / 3 | actual helper PID >2; detail 0 | 1 actual helper pidfd |
| HelperReady / 4 | helper PID >2; detail 0 | 0 |
| Registered / 5 | helper PID >2; detail 0 | 0 |
| Release / 6 | actor PID 2; detail 0 or 1 means the actual helper shape | 0 |
| Released / 7 | actor PID 2; detail 0 | 0 |
| ActorReaped / 8 | actor PID 2; detail >=0, native raw wait encoding | 0 |
| HelperReaped / 9 | helper PID >2; detail >=0, native raw wait encoding | 0 |
| Ack / 10 | PID >=2; detail is an allowed acknowledged kind tag | 0 |
| Failed / 11 | PID >=2; positive errno and a recognized step | 0 |

Only Failed may have a nonzero step. Steps 1–8 are Helper, Setup, Exec,
MountClone, MountAttach, MountClose, ControlLost, ControlProtocol. The separate
launch-status pipe record is step u8 + errno i32 BE, hence `1+4=5` bytes;
`Failure::decode` rejects nonpositive errno and unknown steps. Tags 1–3 preserve
the earlier launch record. **This is the r37 schema.** ROOT9313's newer
`Native` versus `SelectedObjectMismatch` leaf is frozen separately as **F** below.
Its encoding is now inspected; that does not migrate the r37 consumers or give
them the new leaf's codec result.

Acknowledgment matches **both actual PID and original kind**. Allowed original
kinds are ActorBorn, HelperBorn, HelperReady, Release, ActorReaped, HelperReaped.
There is no sequence, Epoch, generation, authority token or receiver FD number
in this packet. Private per-launch endpoints, a single in-flight notification,
and one actual actor/helper association are necessary caller invariants (**R/T**).
The source rejects duplicate/conflicting transitions; this is not replay/isolation
proof for a completed caller. PID 2 derives from the checked first actor in the
fresh namespace, not a general host PID rule. A received integer never grants
authority: the actual owned pidfd supplies the object association.

**M::Status** owns exactly `[u8; FAILURE_BYTES]` and checked fill progress.
EINTR retries, EAGAIN leaves pending, partial EOF is typed Length; five bytes
decode a reported failure. Empty EOF is accepted only with Released already
recorded. That proves the setup-side closure/release condition, not process
liveness, wait consumption or complete population cleanup. **R/T** separately
require matching actual terminal wait facts and sent ACKs. Raw wait status is
not an errno, exit code or frame length. **R** validates WIFEXITED/WIFSIGNALED;
its exit projection uses WEXITSTATUS or checked `128+signal`. Existing public
EXIT's 125–127 and bootstrap `_exit(125)` are command presentation conventions,
not Linux resource limits or a proof that the actor executed.

**C::Error** preserves Native(errno), Closed, OwnerGone, ReaperExited,
HelperExited{status}, Protocol, Codec and Reported(Failure). Protocol reasons
are Length/Rights/Truncated/Conflict/State; codec reasons are
Length/Version/Tag/Fields. errno projections ECONNRESET/EPIPE/EPROTO do not replace
those distinct causes. Failed bootstrap notification is best effort; a full or
lost channel leaves uncertainty, not a fabricated success or received failure.

### Later frozen Failure leaf: same sizes, different typed meaning

Read-only discovery found `P/canonical-failure-v1/`, explicitly marked frozen
and unsigned. Its source is **F**, not a copy of active production code:

| Frozen F input | Bytes / SHA-256 |
| --- | --- |
| `lifecycle.rs` | 16249 / `abaa959c1ca0da0c71caee089f42a03807f523b19b6b663ed5a2a01ff9a6a020` |
| `LEAF.patch` | 16980 / `3469ec55acffd2d9abace3f12fd14ba33619f4ae193b9ab870d6ad3ca0d39e50` |
| `source-pins.json` | 1236 / `f2fa798c29fe88ab17129e186e62850ca844a91a44e2a48d6eb01e8566f2c9ad` |
| `README.md` | 4981 / `75b7cde9745327effc59f9bbe98f0169c65f6c3b927d64e98b9bb23bf183048c` |

All four named file hashes were checked, and the complete leaf/README read.
The patch expects parent082's agreed ABI management/input registration context;
it is not a whole run.rs replacement or a signed current integration identity.

**F::Failure** is `Native(NativeFailure)` or `SelectedObjectMismatch`.
NativeFailure's private Step/NonZeroI32 fields are constructed through checked
positive errno. Tags 1–8 retain their native operations; tags 10/11/12 identify
SelectedPathLookup/SelectedSourceStat/SelectedVisibleStat. Mismatch uses a
separate protocol tag9 followed by exactly four canonical UNUSED zero bytes.
Nonzero mismatch padding refuses; native zero/negative errno refuses. Tag9
is not a native Step, errno zero, resource ceiling or default error classifier.
Its tag space is distinct from the packet kind tags in the earlier table.

Failure remains `1+4=5` wire bytes. `Packet::Failed { pid, failure }` carries the
SAME typed Failure, while `Packet::Control { kind, pid, detail }` cannot encode
Kind::Failed. The packet is `version1 + kind1 + PID4 + Failure5 =11` bytes;
encoding rotates `[tag,a,b,c,d]` into `[a,b,c,d,tag]` at offsets6–10. Native tags
1–3 retain their old literal bytes. Mismatch has bytes6–9 zero and byte10 tag9;
nonfailure packets require trailing tag0. `has_descriptor()` still selects only
ActorBorn/HelperBorn. The new leaf therefore changes neither the one-FD bound
nor CMSG storage derivation, but old r37 `detail/step` consumers cannot simply
be assumed to implement the new union. Their exact migrated caller pin is owed.

The frozen README/source-pins report Mac ARM64 r43 **4 codec tests passed,
0 failed, 0 ignored**, strict production/test Clippy0, same product lock and
original2CPU/2GiB/4GiB/600s/j2/lock/floor. Those are peer receipts read here;
this task neither reran the tests nor audited the raw archive. The named raw
archive identity is 6801B SHA-256
`977cab71dcbb8a69488e7c1f86e586874bae6f043e1f0ef4af16125e12cfaab2`.
No FD/syscall, actual selected-object mismatch, migrated caller, namespace,
actor/helper release, native-error, fault, matrix or performance pass follows.

## Descriptor and storage derivations

**C** allocates payload `[u8; PACKET_BYTES]`, one iovec, and aligned ancillary
storage from `CMSG_SPACE(size_of::<RawFd>())`, rounded up to usize words. On the
supported Linux LP64 amd64/arm64 ABIs: int/RawFd=4, header=16, alignment unit=8,
so `CMSG_LEN(4)=20`, `CMSG_SPACE(4)=24`, and ceil(24/8)=3 words. These are ABI
derivations, not this task's native measurements or a seven-target pass. GNU
uses size_t fields where musl has smaller fields plus ABI padding; retain the
native definitions/casts, not a hand-written universal C struct.
([Linux socket definitions][L-socket], [libc helpers][R-libc],
[GNU layout][R-gnu], [musl header][M-socket])

Ancillary padding can hold a second int: allocating CMSG_SPACE for one FD does
not enforce the one-FD protocol. **C** counts actual installed rights, retains
the first, closes surplus rights, rejects unexpected control data and checks
the exact 0/1 count against the packet kind. On any decode/truncation error it
also closes the retained first descriptor. Linux's per-message `SCM_MAX_FD=253`
is an independent kernel ceiling, not a capacity choice. Linux may install a
prefix, mark MSG_CTRUNC, then dispose of the untransferred references; the
receiver must close its installed prefix. ([SCM ceiling][L-scm-h],
[SCM receive implementation][L-scm]) No source-only leak-free claim follows.

| Storage/object set at the frozen pin | Derivation and limit of the statement |
| --- | --- |
| **M::Launch** descriptor fields before outer clone | executable 1 + acquired kill/events 2 + Owner/Gate/Split/Ready/Setup pairs 10 = **13 stored references**. Initial pidfd -1 is an unacquired sentinel. A transient duplicate writes last_cap. This is not a measured peak, FD entitlement, unique open-file-description count or whole process table. |
| **M::Owned** static descriptor fields | outer reaper pidfd, group, kill, events, Owner writer, Ready endpoint, Setup reader = **7**; actor/helper pidfd slots add at most **2** in this shape. Optional teardown can retire fields earlier. Do not multiply 9 by tenant count and call it a complete admission requirement. |
| reaper fresh-exec allowlist | **5** sources: Owner/Gate/Split/Ready/binary. Destinations 3/4/5/6/7; staging starts at Past=8, above the entire set, duplicates all five before replacement, then closes the tail. CLOEXEC remains on binary. Inherited descriptors and overlap still need actual accounting. |
| helper fresh-exec proposal at 21f | separate roles: null stdio 0/1/2, broker 3, carried server ELF 4 (setup-only), SAME Gate 5 (report then close, no reading actor ACK). These are role assignments in a different process. Six listed destinations do not prove six unique source objects or a measured peak. Shared `child_fds` derives staging from checked max(destination)+1, not a copied minimum. |
| outer **R** slots | one actor, optional one helper, one pending ACK; each task one Pending/Reaped wait slot. ACK write readiness takes precedence over reading another notification. |
| inner **T** slots | one actor, helper Absent/Expected/Known, one early helper fact, one in-flight Ready notification, one Gate response; no retained arbitrary orphan-status history. One nonblocking wait4 per turn is an implemented progress choice, not a measured optimum. |
| poll arrays | **M::Startup** 4 roles: Ready, reaper pidfd, owner, optional Setup; actor association 2: Ready/Owner; **T** 3: Owner/Ready/Gate; **S** 1: Gate. A disabled role uses fd -1, not a new descriptor. New SIGNAL/Pending notifications may alter the actual set; no future bound is asserted. |
| Owner/Split bootstrap | last_cap u32 BE **4** then checked helper bit **1** on Owner; Split fact **1** byte with assigned value 2. Source lengths and values are distinct. Legacy cfg(test) READY=1 byte / five-byte helper-ready records are not the production 11-byte lifecycle protocol. |

Each pending Option is an algorithmic slot, not necessarily 11 bytes of memory:
native Rust enum layout, descriptor owners, stack frames, socket queues,
in-flight rights and inherited descriptors are additional costs. The sources
do not derive SO_SNDBUF/SO_RCVBUF, pending socket capacity or aggregate backing.
Kernel defaults, a page size, Poller batch64 and an observed FD count cannot
supply those missing bounds. No new buffer size is recommended here.

## Stream framing, deadlines and cancellation inputs

Existing `run::header/parse_header` uses kind u8 + three checked zero bytes +
payload length u32 BE = **8 bytes**. These stream frames are not **L** packets.
The existing SIGNAL_PORT 1025 is a protocol rendezvous assignment, not a syscall
limit or descriptor count. TOKEN=16 belongs to the existing exec correlation.
The actual relay dispatch at `init/run.rs` Owner::Signals accepts:

| Existing kind | Payload / full stream frame derivation |
| --- | --- |
| SIGNAL 18 | signal u32 BE: **4 / 12** bytes |
| RESIZE 20 | rows u16 BE + cols u16 BE: **4 / 12** bytes |
| EXEC 21 | TOKEN16 + exec ID u32 + variable Spec: **20+Spec / 28+Spec** bytes |
| EXEC_SIGNAL 22 | exec ID u32 + signal u32: **8 / 16** bytes |
| EXEC_RESIZE 23 | exec ID u32 + Size4: **8 / 16** bytes |
| BOUND 32 | request u32 + errno i32 + port u16 + number u32: **14 / 22** bytes |

BOUND is fixed14, correcting an initial peer message that called it variable.
EXEC is the variable shape. Header length bounds the whole EXEC payload,
including its 20-byte prefix; it is not a bound for Spec plus an extra prefix.
A 12-byte SIGNAL-only allocation cannot preserve this entire reused stream.
Only complete, correctly decoded SIGNAL data has signal meaning. Connection
completion, resize, an unrelated valid frame, readability and EOF are different
events. Do not discard ordinary control bytes to invent startup cancellation.

`frames::each_frame` retains incomplete headers/bodies, checks header padding
and global payload bound, uses checked payload-end addition, and clears malformed
streams. The existing relay appends reads to `from_signals: Vec<u8>` and uses
CHUNK=64KiB scratch; neither that read chunk nor initial Vec::new derives an
admitted startup buffer. Spec includes argv/env and other variable fields.
MAX_PAYLOAD is an existing **protocol acceptance cap** of 6MiB, explained in
source using Linux exec's `_STK_LIM*3/4` bound. Linux also applies current stack
rlimit, ARG_MAX floor and pointer accounting. That exec bound alone does not
prove every non-argv/env Spec field or concurrent retained frame has backing.
([Linux exec accounting][L-exec], [Linux stack constant][L-resource])
No new choice to preallocate 6MiB, 64KiB or 12B follows from those facts.

**C::wait** calls raw SYS_ppoll with a null timeout; EINTR returns to the caller's
progress turn. **T** blocks SIGCHLD outside ppoll and unmasks it atomically in
the wait; one wait4 turn follows notification. Kernel signal-set size is **8**
on the selected 64-bit Linux ABIs, distinct from libc sigset_t storage.
([ppoll implementation][L-select], [generic signal ABI][L-signal],
[amd64 signal ABI][L-xsignal], [arm64 signal ABI][L-asignal])
**M::Startup** borrows only the run stream's hangup; it has no deadline or SIGNAL
parser. **S** waits on Gate; its lifetime failure observation comes through
endpoint closure. Neither r37 nor a syscall source supplies a finite original
startup deadline. If a caller supplies an absolute deadline, its clock/identity,
overflow refusal and remaining-time calculation must survive every interrupted
or partial step; replacing it with a fresh per-step timeout changes the contract.

The frozen proposal `P/STARTUP-CANCEL-INPUT.md` (4367B,
SHA-256 `93378b5c0d439d34e771ecebb033a0db446fc65d7b98b03971c112476de75679`)
correctly identifies the already-dialled signal connection before domains
startup, and proposes borrowing the SAME framing/connection state later used by
relay. It explicitly leaves those extra spans unedited. Its prior
`STARTUP-OWNER-INPUT.patch` (3640B,
SHA-256 `4b4891a38687a8f8d6367708482be32925cb809bb3d65be1d37aa5fbfe6a684f`)
is an unapplied run-hangup-only proposal. The current handoff adds SAME
Pending/Epoch cancel/shutdown notification integration; its immutable source
and caller population have not been delivered. Existing hostbind::Pending is
unrelated bind/listen work, not that lifecycle owner. No bound is guessed from
a same-named type.

## OS constraints and established implementation comparisons

Linux v6.18 [clone_args UAPI][L-sched] has eleven aligned u64 fields, so the
version2 layout is **88** bytes; earlier version minima are 64/80. Existing
`domains::CloneArgs` mirrors those fields. [clone3 implementation][L-fork]
checks size/flags, reserves the pidfd at birth and handles failed construction.
Its PAGE_SIZE upper argument-size check is syscall extension handling, not a
control buffer recommendation. The native helper proposal's CLONE_PARENT,
CLONE_PIDFD, CLONE_NEWNS and exit_signal0 must be checked at its actual trusted
caller; a proposal is not namespace/parenthood proof.

[pidfd_open implementation][L-pid] returns a CLOEXEC process handle;
[pidfd_send_signal][L-pidsignal] still checks eligibility/permissions. pidfd
object identity cannot be replaced by a scalar PID selected from a frame.
[wait UAPI][L-wait] assigns P_PIDFD=3/WEXITED=4/WNOWAIT=0x01000000;
[waitid implementation][L-exit] retains child/wait eligibility. WNOWAIT observes
without consuming; actual reaping remains separate. These are syscall values,
not worker counts, retry counts or queue depths.

[Unix receive implementation][L-unix] preserves record handling for seqpacket;
undersized receive storage reports MSG_TRUNC and discards the uncopied record
tail. Thus **C** treats either MSG_TRUNC or MSG_CTRUNC as refusal, never a partial
packet to concatenate. sendmsg uses MSG_DONTWAIT|MSG_NOSIGNAL; exact11 succeeds,
EINTR retries, EAGAIN retains the SAME packet/original FD for writable readiness;
other short positive sends are typed Length. recvmsg uses MSG_DONTWAIT and
MSG_CMSG_CLOEXEC; EAGAIN is pending, zero is Closed. Peek-based sizing would need
separate rights-ownership proof and is not selected.

systemd v258 provides two established, different mechanisms:

- MAINPIDFD=1 requires exactly one attached FD. Its [service parser][S-service]
  consumes the actual FD into the existing PidRef and applies unit authorization;
  the notification field alone grants nothing. MAINPIDFDID is a different
  existing correlation option, not a reason to add a second Shards identity.
- BARRIER=1 must be the sole assignment with one FD ([protocol][S-notify],
  [manager implementation][S-manager]). The [client][S-daemon] passes a CLOEXEC
  pipe writer, closes its local writer, then waits for reader HUP. That proves
  ordered notification processing under that protocol, not child birth/reaping
  or resource admission. Its timeout starts in the post-send wait; it cannot
  substitute for an original full-operation absolute Shards deadline.

Keep the ROOT-selected explicit pidfd/ACK protocol or compare such ownership
barriers at the existing seam; do not copy systemd's buffer defaults, add an
alternate manager or infer a numerical winner from documentation. Their actual
cost/compatibility and the completed Shards caller remain ROOT proof obligations.

## Actual observation, missing inputs and owed proof

Parent r37's original `pidfd-ack-wait.stdout` and receipt record one native test:
**1 passed / 0 failed / 0 ignored / 4 filtered**, Linux7.2.9-2-cachyos amd64,
glibc2.44, admitted2CPU/2GiB/4GiB/600s/j2. Actual clone PID88206, SCM pidfd
association and matching ACK, received-handle waitid identity, exit23 consumed
by waitpid, then ECHILD; fixture FD4→4. This validates that selected mechanism.
It proves no capacity optimum, namespace PID2 association, actual helper path,
admission/backing, malformed/truncation behavior or new SIGNAL/Pending caller.
The four other compiled tests were filtered, not passed; compile warnings were
not a strict-lint pass. This task reran none of them.

Exact remaining inputs sent to parent `01a1c767840171d5872a1673e3e425f4` and ROOT:

1. An immutable source/patch+base/hash for the new pre-start SIGNAL framing
   owner and SAME Pending/Epoch wake source, all startup/relay borrowers, and
   actual callers migrated to frozen **F**. The Failure encoding input itself
   is resolved above; its caller integration is not. Current r37 results must
   not be silently assigned to later bytes.
2. The real deadline source: original caller clock/absolute expiry or an
   explicit lifetime-unbounded contract; zero/overflow/expiry meanings;
   cancellation priority and how complete noncancel frames remain available.
3. Actual admitted frame shape/population and simultaneous retention: partial
   header/body, pending operation, reply/ACK, old+replacement allocation overlap,
   producer rights and writable/cancel/reaper readiness. MAX_PAYLOAD is not that
   admission. Native layout, allocator capacity and kernel backing are unknown.
4. Real caller proof of one actor/optional one helper per private endpoint;
   helper preparation/registration, retained pidfd through ACK, no Gate reader
   conflict, and all error/drop/owner-loss transitions. More helpers/restarts
   would change the population derivation and need a new contract.
5. ROOT's actual composed guest/native cases: fragmented/extra/unknown records,
   surplus/truncated rights and installed-FD cleanup, stalled queues with
   cancel/reap progress under original deadline, helper/actor/owner exit and
   actual final wait reconciliation. This is an owed proof list, not authority
   to execute a held fault campaign. Parent/kernel accounting, seven targets,
   real VMs, performance/fault/cost comparison remain ROOT-owned and unrun here.

Windows `42e7d6f0fa1ae5372bece2bc93a9673730537fdc` and every r612 failed
controller/source record remain frozen. This document neither executes the
Windows eight cases nor converts r612 FD4→5/final-signal EPERM into a pass.

## Verification and R01–R16 account

Research reads use `git show <exact-commit>:<path>`, read-only tar member reads
and SHA-256 checks. Frozen managed.rs hunks were verified against the old blob
and reconstructed only in memory. External revisions/bytes/hashes follow below;
tag resolution identifies sources, not deployed runtime behavior. A failed
Bash-manual fetch (HTTP400/Timeout) supplied no evidence and is not cited.
`python3 scripts/check-development-policy` returned 0. Its script and policy
inputs match current main. The first in-memory reference/arithmetic document
check returned 1: an extra blank EOF line generated while appending the identity
table violated its final-newline assertion. Diagnostic output established all
24 unique references/rows resolved and no trailing spaces; only that new document
EOF was corrected. No frozen source/raw changed. The final named whitespace and
scalar-derivation results accompany the signed checkpoint. There are no native
test counts for this task.

| Rules | Evidence and remaining limit |
| --- | --- |
| R01 | No executable/production edits. Checked/error behavior above is a focused source observation, not whole-path panic or allocation-refusal proof. |
| R02 | Pinned Linux, libc/musl and systemd primary contracts plus exact Shards owners/callers; no architecture selected without live data. |
| R03 | Every reported packet/field/slot/FD count has a derivation; syscall ceilings, existing defaults, proposed sizes and measured FD results are distinct. Missing new source/population prevents a new numeric recommendation. |
| R04 | Correlation, pending ACK/release/reap and typed causes preserved. Frozen F distinguishes native errno from semantic mismatch without changing byte bounds; new Pending/Epoch and migrated Failure caller source remain unavailable. |
| R05 | Existing owner/framing/readiness reused as the proposal; no cache, spin, polling fallback, duplicate phase owner or manager introduced. |
| R06/R07 | One focused derivation record; comments/code unchanged. Source-state rationale and provenance remain explicit. |
| R08/R14 | No performance/allocation/fault/4–5×/leak-free claim. Actual cost and induced parent/kernel backing are unknown; ROOT owns measurement. |
| R09/R10 | No suspected source concern called reproduced. Existing r37/raw identities and failed/unrun limitations retained; this new document changes no behavior. |
| R11/R12 | Shared Linux guest protocol spans GNU/musl amd64/arm64 under all supported hosts. ABI arithmetic is not actual seven-target/VM validation. Mac ARM64 only; paused WinARM unchanged. No extra deliverable, crypto or host-security change. |
| R13 | Single authorized document, no production rewrite or new abstraction. |
| R15 | Implementer/primary-research role; no children/goals/actors/audits/profiles/main merge. Parent/ROOT receive signed named-path checkpoint and missing inputs; Windows review still separate. |
| R16 | Small read-only source retrieval and one document only; no builds, full worktree/mirror/archive creation, target/cache deletion or protected runner writes. |

## Primary source identities

Retrieved 2026-11-11 UTC as text only, without checkout/mirror. Linux v6.18
resolves to `7d0a66e4bb9081d75c82ec4957c50034cb0ea449`; systemd v258 to
`781d9d0789379d1ea1f2ecefb804d41e9c8b6c38`. libc is pinned to tag0.2.189 and
the fixture's independent registry checksum; musl header to v1.2.5. These are
research pins, not a claim to be the latest releases or the deployed guest.

| ID / pinned source | Bytes | SHA-256 |
| --- | --- | --- |
| [L-sched: include/uapi/linux/sched.h][L-sched] | 6302 | `2081b07cee6adab443e4ba320d0ccb13db4c97bed64f3eefafbfde2f25c833a4` |
| [L-fork: kernel/fork.c][L-fork] | 81495 | `208c4cb84966e179cb8940acc2f4e14d5328d97f62b7d67f04e4353690fb55da` |
| [L-pid: kernel/pid.c][L-pid] | 22671 | `c42a4f56e3939c47b7af58390f98815bd1d64cf423c5be224eed74656e8a7e99` |
| [L-pidsignal: kernel/signal.c][L-pidsignal] | 134558 | `074166696c837c48ba5edc1fcefa4dd3e4b0736b2093545b0824a3f60f9c18e5` |
| [L-exit: kernel/exit.c][L-exit] | 50558 | `0b80d6d1087dc07d8754bf27c402588eda6148d97d5142e075b9e200da46c3fa` |
| [L-wait: include/uapi/linux/wait.h][L-wait] | 697 | `5aef6849976f461a6a202d92c4f37de8d0550b66699efc398ada9e906047fdc3` |
| [L-socket: include/linux/socket.h][L-socket] | 15898 | `be4fced57587c50436643cdcdafc786e4d5628a1dc8992ac0aa552ae6a9bd18e` |
| [L-scm-h: include/net/scm.h][L-scm-h] | 2933 | `cb25a2647fbfec403f71c838bc36c3bccc568d4b00b69452aa8666ee67d207f0` |
| [L-scm: net/core/scm.c][L-scm] | 12706 | `e66ffbd376d67bce8b0f7c12c45ea9fb6a5fb300c20c0fa02a86c08dfe954078` |
| [L-unix: net/unix/af_unix.c][L-unix] | 94154 | `2edb8c05faaaee7bd73f21c225a5f394c21adb2f7eddb7a1c29976c388d6570a` |
| [L-signal: include/uapi/asm-generic/signal.h][L-signal] | 1847 | `936247c76acb82e5c82c58c2cddfe45fcb71230465f6c137966a8a285a3dc04c` |
| [L-xsignal: arch/x86/include/uapi/asm/signal.h][L-xsignal] | 2189 | `4aa8e8c9dd54a2ad66b9f95aba04040bcfeb6046ae5566e13e0dfbd834d271f1` |
| [L-asignal: arch/arm64/include/uapi/asm/signal.h][L-asignal] | 898 | `92c29473269b68c451736ca630be953aa301223328884cb89bc15c016c2eec6b` |
| [S-notify: man/sd_notify.xml][S-notify] | 35822 | `f4dd30c4e1c881c9e3d94e38dbb16793948d8a310494b944521b9c71e79f5608` |
| [S-daemon: src/libsystemd/sd-daemon/sd-daemon.c][S-daemon] | 25377 | `b37e8099dd5a4d699773448a30bb47ad2e85df903dbb0b5cce2530bb5285c278` |
| [S-service: src/core/service.c][S-service] | 238073 | `db3d8ee96682f00a580dfce6a2e12d6294688fc1196e48f0969c7a4f21a76a41` |
| [S-manager: src/core/manager.c][S-manager] | 195462 | `4718d10f14735d1095c3194c41b614bcc66e102283cc3eddc0102dc937639811` |
| [S-io: src/basic/io-util.c][S-io] | 9644 | `2451f7e85ec63e7812714ee804c2b96a2d2fdc8bf5935b637c2f107ccc26019f` |
| [R-libc: src/unix/linux_like/mod.rs][R-libc] | 80253 | `298ca2a15df5313c893184334caef990df5dbe6f71afcf332d4322dde80e1c3d` |
| [M-socket: include/sys/socket.h][M-socket] | 11299 | `1e6f196e7ae112b3295078b45e705b7f69ae2a1ade7f3b72522f66fb695eda65` |
| [R-gnu: src/unix/linux_like/linux/gnu/mod.rs][R-gnu] | 44342 | `df5cb173336a5381f3a668af12c89495fd161bcaa5ff3a46b0f9e7222a3bda2e` |
| [L-select: fs/select.c][L-select] | 35634 | `1d3469c39eb5bd9adef56f7e29d471c15e4fbc98477b39e1d22241ac7e3616dc` |
| [L-exec: fs/exec.c][L-exec] | 51521 | `08569e409280653fe47c1bc44835297cfd9621094bde7cd487f10239b46eff1d` |
| [L-resource: include/uapi/linux/resource.h][L-resource] | 2610 | `42f158b8dbdb7ad125a7d8c294e58a7f1a0a14107522fbaa2bb7fd92f1ba1922` |

[L-sched]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/linux/sched.h
[L-fork]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/fork.c
[L-pid]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/pid.c
[L-pidsignal]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/signal.c
[L-exit]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/exit.c
[L-wait]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/linux/wait.h
[L-socket]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/linux/socket.h
[L-scm-h]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/net/scm.h
[L-scm]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/net/core/scm.c
[L-unix]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/net/unix/af_unix.c
[L-signal]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/asm-generic/signal.h
[L-xsignal]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/arch/x86/include/uapi/asm/signal.h
[L-asignal]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/arch/arm64/include/uapi/asm/signal.h
[S-notify]: https://raw.githubusercontent.com/systemd/systemd/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/man/sd_notify.xml
[S-daemon]: https://raw.githubusercontent.com/systemd/systemd/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/libsystemd/sd-daemon/sd-daemon.c
[S-service]: https://raw.githubusercontent.com/systemd/systemd/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/core/service.c
[S-manager]: https://raw.githubusercontent.com/systemd/systemd/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/core/manager.c
[S-io]: https://raw.githubusercontent.com/systemd/systemd/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/basic/io-util.c
[R-libc]: https://raw.githubusercontent.com/rust-lang/libc/0.2.189/src/unix/linux_like/mod.rs
[M-socket]: https://git.musl-libc.org/cgit/musl/plain/include/sys/socket.h?h=v1.2.5
[R-gnu]: https://raw.githubusercontent.com/rust-lang/libc/0.2.189/src/unix/linux_like/linux/gnu/mod.rs
[L-select]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/select.c
[L-exec]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/exec.c
[L-resource]: https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/linux/resource.h
