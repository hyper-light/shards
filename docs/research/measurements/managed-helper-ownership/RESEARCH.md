# Helper ownership: decision inputs before production edits

Current policy cbd3928 / df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3;
implementer, scoped sandbox helper region and new private evidence. These sources
preceded the production change; the [closing record](README.md) identifies the
actual native baseline, candidate checks and outstanding ROOT acceptance.

The actual caller is Programs::start: a private SOCK_SEQPACKET RUN ID and one
SCM_RIGHTS program socket. A fresh, single-threaded helper serves it, creates
workers and observes their pidfds. The worker installs its stricter filter before
reading the program. Current source forks, acquires pidfd later and pushes a
Worker into an infallibly growing Vec. EOF explicitly kills current entries;
other helper errors and helper SIGKILL have no equivalent lifetime owner action.
These were initially source observations. The native musl fixture subsequently
observed actual QuickJS completion after helper death, as the closing record states.

The worker's program descriptor is dropped before the parent's pidfd_open. Thus
a simple RLIMIT_NOFILE claim from the order alone is not a valid EMFILE
reproduction. The first live case instead independently holds a ready worker
pidfd, ends only its own helper, and checks exit notification plus actual
post-helper-exit QuickJS completion. A second case ends that helper by an invalid
control message. Both use the real sandbox implementation, not a mock worker.

## Primary sources and established alternatives

- Pinned guest Linux v6.18.48 [fork.c](https://github.com/gregkh/linux/blob/v6.18.48/kernel/fork.c):
  copy_process reserves the pidfd and checks put_user before exposing the child;
  failure unwinds that unpublished process. kernel_clone wakes the new task only
  after copy_process succeeds. This supports atomic handle admission rather than
  repairing a fork/pidfd_open gap. Version-pinned guest behavior must still be
  checked live; a newer host alone is insufficient.
- Linux v6.18.48 [sched.h](https://github.com/gregkh/linux/blob/v6.18.48/include/uapi/linux/sched.h)
  defines clone_args, CLONE_PIDFD and the structure-size contract. The
  [clone manual](https://man7.org/linux/man-pages/man2/clone.2.html) describes the
  parent pidfd destination and close-on-exec property. Existing init CloneArgs
  already mirrors this ABI. Server must use the same kernel mechanism on both
  guest architectures; no new host backend or runtime dependency is needed.
- The Linux [PDEATHSIG contract](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html)
  concerns the creating parent thread and only subsequent death. Child-side
  setup alone misses an already-dead parent. Capturing the helper identity before
  creation, arming SIGKILL immediately in the child and comparing getppid closes
  that setup race for this single-thread helper. Worker seccomp must then deny
  resetting the setting or forking; it performs no credential-changing exec.
  Linux v6.18.48 [sys.c](https://github.com/gregkh/linux/blob/v6.18.48/kernel/sys.c)
  takes tasklist_lock while changing pdeath_signal so that subsequent getppid
  reflects an already-dead parent or reparenting sends the selected signal.
- [runc v1.4.0 standard_init_linux.go](https://github.com/opencontainers/runc/blob/v1.4.0/libcontainer/standard_init_linux.go)
  restores the parent-death setting after credential setup and compares the
  original parent before exec. Its broader container bootstrap is unnecessary
  here; the narrowly applicable race and credential obligations remain relevant.
- [youki v0.6.0 fork.rs](https://github.com/youki-dev/youki/blob/v0.6.0/crates/libcontainer/src/process/fork.rs)
  uses a raw clone3 ABI and an ENOSYS legacy-clone fallback, and records libc
  bookkeeping concerns. Copying its no-pidfd callback fallback would discard this
  task's atomic ownership requirement. This helper runs freshly exec'd with no
  other thread; no allocator entry after a multithreaded fork is proposed.
- [libc 0.2.189 clone_args](https://docs.rs/libc/0.2.189/libc/struct.clone_args.html)
  already exposes the aligned Linux UAPI type with every required field. The
  first inspected Linux submodule lacked it, which was insufficient evidence of
  absence from the crate. Complete published-package search (verified against the
  repository checksum) finds declarations for both musl guest architectures.
  [Two exact lightweight typechecks](libc-uapi-typecheck.json) now pass. Reuse
  libc::clone_args; do not add another CloneArgs, shared ABI module or dependency
  merely because init currently carries its own equivalent layout. No guessed
  syscall/flag constant. This is type evidence, not native clone3 execution.

The other considered choice is legacy clone(CLONE_PIDFD), which can also return
an atomic pidfd but has architecture-dependent argument ordering; clone3 fits
the repository's existing guest mechanism. A newer autokill-on-close flag is not
in the pinned v6.18.48 ABI, so it cannot silently replace a supported kernel
contract. pidfd_open after fork retains the demonstrated ownership gap; shell
supervision/global process polling would add authority and inference rather than
close it.

The [pidfd_open manual](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)
lists descriptor and kernel-memory failures and the conditions under which an
unreaped fork child remains identity-stable. Those conditions do not reserve the
handle before birth. The [wait manual](https://man7.org/linux/man-pages/man2/waitid.2.html)
distinguishes an exit notification from reaping: this fixture captures waitpid
status for each independently held owned worker. These effects do not establish
all kernel charge return. The [fork manual](https://man7.org/linux/man-pages/man2/fork.2.html)
also records reset parent-death settings and the multithreaded-child runtime
restriction; no new threaded fork path is proposed.

## Selected narrow mechanism

Reserve Worker and next pollfd capacity fallibly before birth, use clone3 with
CLONE_PIDFD, arm/check parent death before worker code or allocation, and retain
one authoritative Worker owner through kill/reap/error paths. Reuse the existing
control wire and actual worker, without new execution authority or a second
manager. Build response packets on the stack from their existing wire width;
remove allocating ended-index collection by traversing readiness in reverse.
Every failed reservation/creation refuses before untrusted execution; a helper
error ends owned workers while keeping failure/cleanup uncertainty explicit.

Initial Linux admission refusals launched nothing. Subsequent native musl
before/after and mutation-driver results are in the closing record. Complete
source/type and native evidence remain distinct; ROOT owns final integration.
