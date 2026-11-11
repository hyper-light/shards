# Narrow native birth access producer

ROOT selected this implementation after the disposable guest observations below.
Policy revision 06ae067c0beda71c98a8c6f9633df9feb71fc11b, coordinator;
rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
This is required new native executor work, not a reproduced legacy defect.

Linux checks the current clone caller's filesystem credentials at destination
and common-ancestor cgroup.procs inodes. A privileged opener of the directory FD
does not supply those credentials. The actual source graph places the outer
member in M before helper birth; helper inherits M, drops all capabilities and
uses normalized Domain.id. The actual job is a direct child J of that same held
M. Therefore the two inode checks are M/procs and J/procs. Kernel LSM, namespace,
topology, controller and resource predicates remain independent.

Sources: [Linux 6.18 cgroup.c](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L6689-L6763),
[ancestor checks](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c#L5292-L5357).
The full local primary research compares separate helper credentials and
owner-side birth in managed-native-cgroup-birth-research-v1.md. Separate credentials
would require a lawful child credential transition by a capability-free helper.
Owner-side birth changes namespaces, parentage and wait ownership. Copying
[systemd's subtree delegation](https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/shared/cgroup-setup.c#L194-L242)
would also grant directory/controller access. Those broader changes are not needed
for the observed two-inode predicate. [runc's parent placement](https://github.com/opencontainers/runc/blob/d842d7719497cc3b774fd71620278ac9e17710e0/libcontainer/process_linux.go#L651-L667)
is an established alternative but uses a different placement/ownership sequence.

The selected producer derives the principal from the original protected normalized
Domain, matches its original Scope.member and exact Staged group component, checks
cgroup-v2, and acquires retained INIT-only member/procs descriptors before changing
access. It changes only the fixed NOFOLLOW procs inode's UID, preserves its actual
GID/mode, and checks identity/readback. Group/other write is refused. The same
retained original member derives each new job; its procs access is installed
before that job's Prepared can be published. Existing Group cleanup retains failed
removal and typed permission failures. No new manager, controller, public field,
UID source, capability, timer, cache, queue or fallback is introduced.

The Member object owns the actual installed access and retained descriptors. It
does not grant process/file operations, establish current ownership, create a
Session, reserve ancestor resources or promise hard backing. Current rights and
commit still use the existing Owned turn. Its two retained descriptors and a
temporary job-procs FD are actual induced costs; capacity/backing and cost proof
remain ROOT obligations. The helper receives only the actual per-job directory;
the actor receives no member/job/procs FD. Same-UID actor/helper DAC cannot
distinguish them. Actual path/FD exclusion and untrusted syscall restrictions
remain necessary and are not proved by these controls. Landlock alone does not
intercept the clone path's inode permission check; do not infer leaked-FD safety.

The guest-only probe uses an actual rootless host KVM run of pinned 6.18.48
kernel48a3824b, guest PID1 creates disposable M/J/sibling groups, and the helper is
UID/GID200000 with zero effective/permitted capabilities. Five controls observed:
privileged directory opener alone EACCES; J/procs alone EACCES; M+J/procs allows
atomic child placement 0::/member/job with actual pidfd-ready/wait/ECHILD; an
unrelated sibling destination EACCES; restoring M ownership restores EACCES.
Actual job/member populated0, helper reap, exact removals and unchanged directory/
subtree ownership/modes were checked. Both procs owners/modes were restored.
These are mechanism observations, not the complete native producer/actor proof.

The first stopped fixture incorrectly expected a failed clone's pidfd output
slot to remain -1. Actual EACCES left numeric7. Pinned fork.c stores that number
before cgroup_can_fork and releases the reserved descriptor on failure. The new
fixture records F_GETFD/EBADF instead; it never owns/closes a descriptor based on
that number. First source, binary and stopped stdout remain retained. This is a
test-contract correction, not a production bug claim. The corrected five-case
run, exact source/kernel/host binaries, direct wait/FD receipts and all remaining
uncertainties are retained under raw. No held campaign or host permission was
changed. Actual production helper/actor, ancestor resource entitlement, active
record/body/parser capacity, supported-target and performance gates remain open.
