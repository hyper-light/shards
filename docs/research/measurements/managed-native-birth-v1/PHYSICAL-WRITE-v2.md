# Trusted Write child: physical lifetime and semantic rights

ROOT decision receipts13252/13253, 2026-11-11. Required new implementation,
not a reproduced legacy defect or a passing native effect result. Current policy
main0d261aa93, rulesdf2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3;
coordinator ROOT. This supplements the actual narrow Member producer in3a47aee7.

The new Write implementation must not perform potentially blocking pwrite in
the shared helper control loop. Linux's regular-file write implementation takes
inode/page-cache locks, allocates/faults and may perform synchronization; regular
file readiness does not provide a universal immediate-I/O guarantee. The
[pwrite contract](https://man7.org/linux/man-pages/man2/pwrite.2.html) preserves
explicit offsets and partial counts; it does not promise nonblocking completion.
The [Linux6.18 implementation](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/filemap.c)
defines generic_file_write_iter/generic_perform_write and synchronization. The
[epoll interface](https://man7.org/linux/man-pages/man2/epoll_ctl.2.html) explicitly
reports unsupported target types, including regular files. This is a new-feature
control-survival requirement, not an observed shipped helper stall.

Reuse the SAME Worker, actual Group, clone/pidfd, association/ACK, private gate,
current owner, wait and population path already selected for native work. Keep
CodeWorker's filter unchanged. The trusted File worker adds only its required
pwrite64 call to the existing worker filter and remains constrained by inherited
owner policy. Actual composed filter/FD/credential behavior remains a ROOT gate.
No new manager, thread pool, queue, namespace per logical delegation or installed
binary is introduced. A new asynchronous I/O subsystem would add registration,
pinning and kernel-state obligations; a new thread pool would add another
lifetime/accounting seam. Neither is selected for this implementation.

Physical child acquisition is separate from permission to execute a requested
untrusted program. The [existing authority decision](../../../managed-native-authority-decision.md)
already distinguishes required trusted management workers from the owner's
semantic process grant. processes=none continues to deny Effect::Process.
An independently authorized Write still needs actual lifetime resources for its
trusted child. The parent derives this mechanics requirement from the canonical
prepared descriptor contract for the actual effect and captured Scope/operation;
it independently reselects original object rights and current owner. A descriptor
kind, reported fact or writable file does not grant the effect.

The earlier checked v7b file-state model used no physical child and therefore
passed no birth object for Read/Write. Preserve that exact source/result as
history. NEW Write preparation must acquire the same Member-derived Group before
publication, permit actual Associate and retain the physical child until matching
trusted reap plus kernel empty (or actual trusted NotBorn plus empty before any
birth). Its semantic effect remains Write. Complete, accepted prefix, launch
failure, wait and retirement stay independent. No Complete/EOF/pidfd-readiness
shortcut removes a physical lifetime or returns a charge.

Canonical native source v2 is pinned by manifest
84a06138173ec7d21c5dde92d96cc5eacac69e6aea60fd23cb6e128f6c4f2a9c,
operation2690f4a88c0693ef78e9c9e050ec84cbb8b5c0dca7e5807ebc683643e76b6b66.
Write Prepared expects the actual Group. Continue8/Continued16 carries the
observed accepted-byte prefix on the same original identity. The prefix is a
lower-bound fact and range check, not a replay guard or authority. Each admitted
attempt requires fresh current/root checks and a distinct live permit consumed
once through the existing explicit flight state. Concurrent pending/refused/
delivered/terminal states must remain explicit. A repeated numeric prefix alone
cannot distinguish no-progress retry from a duplicate.

The exact writerccd7 source currently treats negative returns, including EINTR
and EAGAIN, as terminal Failed(saved errno), and zero as WriteZero. This cohort
does not retry no-progress attempts. A future retry would need fresh current ACK
and retired-permit state, not reuse of an earlier ACK or a strictly-increasing
count assumption. Positive partial writes retain their exact accepted prefix;
later current refusal or lost terminal evidence cannot erase it or synthesize a
successful whole write. Native owns the canonical codec/Worker; the runtime owner
owns the same-Owner physical and continuation consumer. Both reported this exact
split, with no active shared-file overwrite.

No new source-gate result follows from this decision. Actual effects, audit
intent-before-release, actor FD/path exclusion, syscall/LSM conditions, population,
finite records, parser/reply/body/FD/transport and ancestor backing, seven targets,
adversarial and comparative costs remain open. Old worker source/template lock
f4dd is not the native product lock1ecb; exact dependency provenance must remain
separate. The held audit4819 graph and five portable cases are unchanged. A
replayed coordinator floor-restoration notice is not fresh person authorization.
