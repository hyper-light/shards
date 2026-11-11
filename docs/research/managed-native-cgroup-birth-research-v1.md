# Native cgroup birth: Linux 6.18 permission prerequisites

Finite primary-source follow-up for ROOT, 2026-11-11. **A cgroup directory FD
identifies the destination; it does not carry its privileged opener's authority
into `clone3(CLONE_INTO_CGROUP)`. Linux checks the caller's current credentials
against destination and common-ancestor `cgroup.procs` inodes.** Ordinary writes
to an already-open `cgroup.procs` use a different credential/namespace path.
[Owning clone path][clone], [migration path][migration]

No architecture, permission change or capability retention is selected here.
This is source research, not a Shards audit or a demonstrated guest result.

## Scope and supplied starting facts

POLICY_READ: implementer/researcher `/root/record_backing_research`; coordinator
ROOT; base/current `origin/main` `06ae067c0beda71c98a8c6f9633df9feb71fc11b`;
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Read the rewritten `/Users/adalundhe/g121-handoff.md` in full, SHA-256
`679c77583a3fb4fe11a7b3b166d5fa5a1db6c327731d328f3c371c5fa5e37f22`, and
re-read AGENTS/R01–R16/evidence. CLAUDE/plan/authority documents were read fully
at this same revision in the preceding batch. Own only this new note. No source,
build, runtime, actors, permissions, cgroup writes, children or signer invocation.
The already-staged backing note remains SHA-256
`440ef9b6a46edff94a5043252ecfbb2ff89939e9e03c8e6ab1c195c9747781a7`.

ROOT supplied these independently inspected v3 facts; this lane did not re-audit
them: the outer domain birth enters original member **M**; the helper's later
birth has no cgroup move, so it inherits M. Helper/actor confinement selects the
same normalized member UID/GID and clears capabilities, with no-new-privileges.
Fresh job **J** is below that same held M; no child controllers are enabled.
Cgroup filesystem paths and management FDs are excluded from the actor's view.
The analysis below is conditional on those facts remaining true at actual birth.

## Exact checks: directory FD birth versus procs-file migration

| Path | Credential and namespace used | Required predicates and limits |
| --- | --- | --- |
| `clone3(CLONE_INTO_CGROUP, cgroup=directory_fd)` | `task_css_set(current)` supplies source **S**. Destination is resolved from the supplied FD, but checks use current credentials and `current->nsproxy->cgroup_ns`. | FD must identify an online, non-dead cgroup-v2 directory; an O_PATH FD is acceptable, a `cgroup.procs` FD is not. `cgroup_may_write(J)` and `cgroup_attach_permissions(S,J,...)` must succeed. No opener-credential override occurs. [Code][clone], [v2 resolution][directory] |
| `write(open_procs_fd, pid)` | The callback overrides credentials with `of->file->f_cred`; its namespace is retained at file open. Source is the selected target task's current cgroup, not necessarily the writer's. | Destination write access was acquired by opening the writable file. The callback checks common-ancestor permission under those stored credentials; it does not repeat the clone path's destination-inode check. This does not bypass actual-write LSM checks, which run before the callback. [Open context][file-context], [migration][migration], [write checks][vfs-write] |

`cgroup_may_write` obtains the actual `cgroup.procs` inode from the FD's
superblock and calls `inode_permission(&nop_mnt_idmap, inode, MAY_WRITE)`.
Kernfs refreshes inode attributes and calls generic DAC permission checking;
Linux uses fsuid/fsgid, group membership and applicable inode permissions, with
capability overrides only when actually available. `inode_permission` also
performs superblock/immutable/unmapped-ID checks and invokes the inode LSM hook.
An earlier privileged open of the directory does not replace these current
checks. [Cgroup check][permission], [kernfs][kernfs-permission], [DAC][dac],
[inode/LSM dispatch][inode-permission]

The common ancestor is found by walking upward from S until J is its descendant.
That ancestor's `cgroup.procs` must also be writable. With the supplied actual
S=M and J below M, this is **M/cgroup.procs plus J/cgroup.procs**, not a guessed
global/root ancestor. If helper placement changes, recompute it. Neither these
checks nor the kernel's delegated-subtree containment authenticate a Shards job,
original scope, current owner or commit. [Implementation][permission],
[delegation containment][containment]

With `nsdelegate`, both S and J must descend from the applicable cgroup namespace
root, or the check returns ENOENT. This is an ancestry test, not proof that the
caller can resolve their filesystem paths. Clone uses the current namespace;
ordinary procs migration uses the opener's retained namespace. A cgroup namespace
alone is not a substituted permission grant; the boundary check is conditional
on the hierarchy's `nsdelegate` flag. [Check][permission], [open context][file-context]

The destination must also be a valid resource domain. For ordinary non-threaded
domains, nonempty `subtree_control` refuses process placement with EBUSY; invalid
domains can return EOPNOTSUPP. Controller `can_fork` callbacks can still refuse
after permission checks. Thus "no child controllers enabled" addresses one
predicate, not all resource, topology, allocation or lifecycle conditions.
[Destination checks][destination], [fork callbacks][can-fork]

## Landlock and SELinux: consequences supported by these paths

**Landlock:** Linux 6.18's filesystem hook table registers file-open and path
hooks, but no `inode_permission` hook. This clone path obtains an inode directly;
it does not open `cgroup.procs` through `file_open`. Consequently, a Landlock
filesystem rule cannot be assumed to deny this clone merely because opening
the corresponding path would be denied. The file-open implementation itself
notes O_PATH's zero access request. These are code-path findings, not a claim
that any current Shards actor can obtain such an FD. [Cgroup path][clone],
[Landlock hooks][landlock-hooks], [open implementation][landlock-open]

Landlock also documents that pre-existing or passed file descriptors preserve
their associated properties; ordinary read/write permissions are checked on
open rather than each subsequent call. A privileged pre-opened procs file is
therefore not interchangeable with a directory FD or a safe way to grant only
one birth. No existing actor/helper Landlock rules were evaluated here.
[Descriptor semantics][landlock-fds], [different cgroup write path][migration]

**SELinux:** its inode-permission hook evaluates the current SID against inode
class/label permissions. It can independently deny the clone's MAY_WRITE checks
when active/enforcing; changing DAC ownership/mode alone does not establish
success. Ordinary writes also pass the file-permission hook, which revalidates
current credentials when needed, including descriptor-use permission when the
SID differs from the opener's. Fork has a separate PROCESS__FORK check. Actual
LSM activation, labels, policy and enforcing/permissive state are unverified;
no allow rule or label transition is proposed. [Inode policy][selinux-inode],
[file permission][selinux-file], [FD use][selinux-fd], [fork][selinux-fork]

## Three producer alternatives; none selected

| Alternative | Precise prerequisite | Limitation ROOT must resolve |
| --- | --- | --- |
| Narrow permission for the original helper principal | For unchanged S=M, current helper filesystem credentials must pass write checks on only M/procs and each actual J/procs, plus LSM/namespace/topology/fork conditions. The clone check does not require delegating directory creation/removal or controller files merely to use the already-held J directory FD. This narrower statement is an inference from the enumerated kernel predicates, not full subtree delegation. [Predicate][permission], [clone][clone] | Helper and actor share UID/GID in the supplied graph: these inode DAC grants cannot distinguish them. Actual denial of actor path/FD acquisition and invocation remains essential. A held O_PATH directory can reach this clone path without reopening procs; filesystem pathname concealment alone is not evidence about leaked FDs. Rights are not limited by the kernel to one Shards operation. No chmod/chown recipe is recommended. |
| Separate trusted helper principal | That principal must satisfy the same two inode checks for its actual S/J. A distinct filesystem identity can separate DAC access if mappings, groups, modes and capabilities actually do so. | Children inherit helper credentials. A capability-free helper cannot arbitrarily change a child to another UID: `setresuid` restricts new IDs absent actual CAP_SETUID. Expected job credentials, held-object access and all credential transitions need a lawful producer and proof; this note does not recommend retaining capabilities. A passed writable procs FD still has opener-credential semantics. [Inheritance][credentials], [setresuid][setresuid], [migration][migration] |
| Existing protected root owner performs atomic birth | The actual owner caller supplies J to `CLONE_INTO_CGROUP`, satisfies its own current-credential checks, and owns trusted pre-exec setup/association. Its source/common ancestor is recomputed from the owner's actual cgroup; M is not inherited as a permission premise from the helper. [Clone][clone] | Parentage, wait ownership, namespace/FD setup and release protocol change. PID1/global or container init cannot simply retain CLONE_PARENT: Linux rejects it for SIGNAL_UNKILLABLE callers. Other clone flags have independent requirements; e.g. new mount namespaces require CAP_SYS_ADMIN in the relevant user namespace. A later procs migration is a different mechanism, not evidence of atomic birth. [Parent flag][clone-parent], [namespace permission][new-namespaces] |

## Established delegation and placement precedents

- **systemd v258** documents exclusive single-writer ownership and explicit
  subtree delegation. Its `cg_set_access` changes the cgroup directory and an
  allowlist including procs, subtree_control, threads and selected memory files.
  That is broader management delegation than the two-inode birth predicate;
  copying it would grant additional operations. The documented model leaves
  parent resource controls outside delegate authority.
  [Ownership][systemd-ownership], [actual access setter][systemd-access],
  [kernel delegation model][delegation]
- **runc v1.3.3**, using cgroups v0.0.4, selects OwnerUID for a private cgroup
  namespace with writable cgroupfs. Its systemd-v2 manager chowns the directory
  and files enumerated by `/sys/kernel/cgroup/delegate`; not every manager
  supports that ownership field. This is explicit full-subtree access policy,
  not proof that an acquired directory FD confers clone authority.
  [Selection][runc-owner], [ownership implementation][runc-chown],
  [field boundary][runc-owner-field], [dependency pin][runc-pin]
- The same runc parent applies cgroup placement before synchronizing with the
  child; its fs2 manager opens/writes cgroup.procs. This is an established trusted
  parent placement pattern. It uses post-birth migration, so it does not itself
  validate atomic `CLONE_INTO_CGROUP` or Shards' existing parent/wait arrangement.
  [Parent order][runc-order], [manager][runc-apply], [actual write][runc-write]

## Predicates ROOT must establish before release

The actual caller must have the intended fsuid/fsgid/groups/user-namespace
mapping and zero/retained capabilities exactly as selected; its live source
cgroup and cgroup namespace must match the permission calculation. Verify the
held destination's actual identity, liveness, v2 hierarchy, procs ownership/mode,
effective LSM decision, valid domain/subtree state and ancestor permission.
The opener's success is not a substitute. A change of caller/principal/source
invalidates a calculation made for the previous one.

ROOT's authorized live proof must observe real atomic destination placement,
correct pidfd/parent/wait ownership and no untrusted instruction before original
scope/current commit/setup gates. It must observe denied unintended destinations
and actor access to cgroup paths, directory/procs FDs, transferred/inherited FDs
and procfs access as applicable, alongside unchanged controller/parent ownership.
Permission errors must retain correct NotBorn/possible-birth/Unknown ownership;
no EOF/refusal/close alone proves population cleanup. These are verification
requirements, not results or authorization to resume a held isolation campaign.
ROOT retains the actual producer decision, source audit and VM/isolation tests.

## Sources, checks and policy account

Primary GitHub HTTPS files were read at Linux v6.18
`7d0a66e4bb9081d75c82ec4957c50034cb0ea449`, systemd v258
`781d9d0789379d1ea1f2ecefb804d41e9c8b6c38`, runc v1.3.3
`d842d7719497cc3b774fd71620278ac9e17710e0`, cgroups v0.0.4
`a3e2ecd1f756a19cee15f85b96337a59c3b5337b`. These resolved pins are not a
claim about latest versions or the effective guest's patch/configuration.
Default-branch search found file locations only; every cited implementation was
then read at its stated immutable revision. Reproduce with each link's path and
numbered lines. No guest/controller/permission syscall was executed.

| Rules | Evidence and remaining boundary |
| --- | --- |
| R01/R03/R04 | Documentation only; no production panic path, policy constant, credential transition or typed refusal implemented. |
| R02/R05/R12 | Owning kernel/LSM paths and established alternatives cited. No selected architecture, new manager/cache/polling/OS adapter or permission change. |
| R06/R07/R13 | One scoped note; supplied facts, source predicates and conditional inferences remain separate. |
| R08/R14 | No performance/profile/allocation/leak-free claim; ROOT owns measurement. |
| R09/R10 | Source findings are not reproduced Shards defects or live success. Revisions and precise source locators retained; real/mutation evidence remains ROOT's work. |
| R11 | Linux guest scope; seven supported targets remain required, macOS arm64 only; paused WinARM untouched. |
| R15/R16 | Implementer research only, no children/audit/build/actors/cleanup/cache/main changes; prior staged note preserved; unsigned new document returned for ROOT's own signer. All holds persist. |

Mechanical policy validation uses the unchanged base script's `check(root)`
against read-only Git objects, as in the preceding note; exact revision and
outcome, citation/whitespace checks and new file hash are returned with the note.
These checks do not replace ROOT's full-checkout gates or live evidence review.

[clone]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L6689-L6772
[directory]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L6634-L6670
[migration]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L5360-L5410
[file-context]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L4251-L4309
[permission]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L5292-L5357
[kernfs-permission]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/kernfs/inode.c#L276-L294
[dac]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/namei.c#L443-L506
[inode-permission]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/namei.c#L557-L601
[vfs-write]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/read_write.c#L451-L477
[containment]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L571-L610
[destination]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L2793-L2822
[can-fork]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L6814-L6852
[landlock-hooks]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/security/landlock/fs.c#L1831-L1862
[landlock-open]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/security/landlock/fs.c#L1576-L1649
[landlock-fds]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/userspace-api/landlock.rst#L367-L384
[selinux-inode]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/security/selinux/hooks.c#L3193-L3251
[selinux-file]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/security/selinux/hooks.c#L3762-L3792
[selinux-fd]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/security/selinux/hooks.c#L1750-L1784
[selinux-fork]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/security/selinux/hooks.c#L4151-L4159
[credentials]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cred.c#L290-L345
[setresuid]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/sys.c#L708-L776
[clone-parent]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/fork.c#L1964-L1972
[new-namespaces]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/nsproxy.c#L147-L177
[systemd-ownership]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/docs/CGROUP_DELEGATION.md#L55-L74
[systemd-access]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/shared/cgroup-setup.c#L194-L242
[delegation]: https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst#L536-L564
[runc-owner]: https://github.com/opencontainers/runc/blob/d842d7719497cc3b774fd71620278ac9e17710e0/libcontainer/specconv/spec_linux.go#L499-L533
[runc-chown]: https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/systemd/v2.go#L338-L383
[runc-owner-field]: https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/config_linux.go#L45-L50
[runc-pin]: https://github.com/opencontainers/runc/blob/d842d7719497cc3b774fd71620278ac9e17710e0/go.mod#L17-L18
[runc-order]: https://github.com/opencontainers/runc/blob/d842d7719497cc3b774fd71620278ac9e17710e0/libcontainer/process_linux.go#L651-L667
[runc-apply]: https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/fs2/fs2.go#L64-L83
[runc-write]: https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/utils.go#L380-L414
