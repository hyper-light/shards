# musl-opath-v1: init's path descriptors on musl, and code mode's filters through a stop

Implementer lane `p/musl-opath-v1`, from origin/main `92409d138`; landed on main at
`fc71859bc` (a merge of main `d13253e4b`). POLICY_READ implementer: AGENTS.md,
docs/development-rules.md (R01–R16, SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`) and
docs/development-evidence.md read in full from origin/main; CLAUDE.md. Coordinator: ROOT
(kiln agent 01a148884a4770fdaf0111f3f792a248). Task: the two latent defects PM M525 found
(docs/research/measurements/ci-green-arm64-v1, items 2 and 3), recorded as PM M531. The
person's rule of 2026-11-12 has implementers land their own work once its unit and light
checks pass.

Hosts: x86_64 Linux is adacachy (CachyOS, kernel 7.2.9, Docker 29.9.0), natively (glibc)
and in CI's pinned musl image (`rust:1.98.1-alpine@sha256:7cc1c22d…`, Docker's default
seccomp profile, the checks as the checkout's owner, uid 1000, no capabilities:
`harness/container-musl.sh`, after scripts/musl-ci). arm64 Linux is Docker Desktop's VM on
an Apple silicon Mac (adalundhe-macbook-pro, kernel 6.12.76-linuxkit): the same musl image,
and Debian's `rust:1.98.1` with seccomp off (`harness/container-gnu.sh`), each as uid 1000.
No arm64 Linux runner is here, and neither Mac Studio's Docker answered (a Lima VM's image
download ran at 200 KB/s and was abandoned). Each host ran the branch's head cloned from
GitHub: kiln's sync of this worktree to adacachy2 and to a Mac Studio did not finish.

## 1. init's O_PATH opens were read-only opens on musl

- **Where.** Every `custom_flags` of the workspace's Rust (crates/*/src, tests, benches):
  the ones asking `O_PATH` were init's alone, four in production and one in a test:
  `changes::keep` (the run's layers, for diff, size, commit and D190's go-over), join.rs's
  `Kept` (a joiner's layers, D119) and `put_back`'s hold of its root, run.rs's `/proc` for a
  joiner's changes, and join.rs's `a_report_says_the_users_and_layers_or_why`. Every other
  `O_PATH` open (init's inroot.rs, domains.rs, devices.rs, setup.rs, managed.rs, over.rs and
  changes.rs's `written`; shards-archive; the VMM's virtio-fs server; the VM process's
  confine.rs; shards-server's test fixture after M525) is open(2), openat(2) or openat2(2)
  itself, which musl passes as given. shards-server and testguest had none.
- **Cause.** std builds its open flags as `O_CLOEXEC | access | (custom_flags &
  !O_ACCMODE)` (library/std/src/sys/fs/unix.rs `File::open_c`, read in Rust 1.94.1's and
  1.100 nightly's sources) and musl's `O_ACCMODE` is `03|O_SEARCH`, its `O_SEARCH` being
  `O_PATH` (musl include/fcntl.h; libc 0.2.189 `O_ACCMODE = 0o10000003`). init is built for
  musl alone, so each was an `O_RDONLY | O_DIRECTORY` open in every guest.
- **The symptom, live** (R09). `harness/probes/keep.rs` runs the production `changes::keep`
  in init's test binary and reads its descriptors back (F_GETFL), then keeps a layer of
  mode 0. With `changes::keep` as main had it, on musl: neither descriptor is `O_PATH`
  (`O_DIRECTORY | O_LARGEFILE`: x86_64 `0o300000`, arm64 `0o440000`), and
  the mode-0 layer is refused (EACCES, uid 1000); on glibc both pass, as glibc's
  `O_ACCMODE` (3) keeps `O_PATH`. With the fix, on each: `0o10200000` on x86_64 and
  `0o10040000` on arm64 (`O_PATH | O_DIRECTORY`), and the layer is kept. The helper put back
  on std's flags (`helper-as-std`) gives the same read-only directory (`0o300000` on
  x86_64 musl).
- **What else differed** (Linux v7.2-rc4 fs/open.c). A path open sets `FMODE_PATH`, empty
  file operations and `FMODE_NONOTIFY` and returns before the file system's open
  (`do_dentry_open`), and asks no permission of the file (`build_open_flags`: no access
  mode). The read-only opens ran the directory's file system's open and release (an
  overlay's, as put_back's root is the run's overlay, allocates its state and opens the real
  directory beneath: fs/overlayfs/readdir.c `ovl_dir_open`), raised fsnotify's open and
  close events, asked for read permission, and gave a descriptor that can read the
  directory and take `fgetxattr`, `ioctl` and `fchmod`, where a path descriptor only names
  it. Each was an `O_DIRECTORY` open, which refuses what is not a directory (ENOTDIR) before
  any open, so no FIFO or device was opened by them. init runs them as root, which reads any
  directory: no failure in a guest is known, and none is claimed.
- **Fix.** `crates/init/src/opath.rs`: `opath::open(path, flags)`, open(2) with `flags |
  O_PATH | O_CLOEXEC`, retried on EINTR as std's open is; the five opens call it.
  `run.rs`'s `read_at` takes the descriptor as an `OwnedFd`.
- **Tests.** `opath::tests::what_it_opens_is_a_path_descriptor`: a directory, a file and a
  mode-0 directory each opened `O_PATH` and close-on-exec by F_GETFL and F_GETFD, and
  `O_DIRECTORY` refusing a file. `opath::tests::no_source_asks_std_for_a_path_descriptor`:
  no source of init's (comments aside) calls `custom_flags` with `O_PATH`; it fails on glibc
  too, where the trap passes every other test.

## 2. Code mode's workers were killed resuming a timed wait

- **Cause.** The worker's filter allows `futex`, any operation, any timeout, and not
  `restart_syscall`. A timed `FUTEX_WAIT` a signal breaks into leaves `futex_wait_restart`
  in the task's restart block and returns `-ERESTART_RESTARTBLOCK` (Linux v7.2-rc4
  kernel/futex/waitwake.c `futex_wait`); where no handler runs (a stop, a cgroup's freeze, a
  tracer), the kernel resumes it by `restart_syscall` (arch/x86/kernel/signal.c `regs->ax =
  get_nr_restart_syscall`; arch/arm64/kernel/signal.c `setup_restart_syscall`, x8 =
  `__NR_restart_syscall`), on both architectures, unlike the helper's poll, which only
  x86_64's `poll(2)` resumes so (fs/select.c `do_restart_poll`; arm64 has ppoll alone,
  which restarts as itself). `restart_syscall` calls only `restart_block.fn`, which the
  interrupted call set, else `do_no_restart_syscall` (EINTR) (kernel/signal.c): it allows no
  other call.
- **The symptom, live.** `sandbox::tests::a_worker_stopped_in_a_timed_wait_lives_on` forks a
  child under the worker's own filter (`allowlist(&worker_calls())`), which waits on a
  shared futex word with a 30-second timeout; once /proc says it sleeps, it is stopped
  (SIGSTOP; `waitpid(WUNTRACED)` sees the stop) and continued, then woken. With the filters
  as main had them (`restart-helper-only`): `ended by signal 31`, SIGSYS, on x86_64 and
  arm64, glibc and musl. With the fix: it exits 0 on each. The helper's own test
  (`the_helper_stopped_in_its_poll_lives_on`) dies so only with `restart_syscall` in neither
  filter (`restart-none`), and on x86_64 alone: arm64's ppoll restarts as itself.
- **Reached today?** No production worker path is known to make a timed wait.
  `harness/probes/atomics.rs` runs a program's `Atomics.wait(a, 0, 0, 50)` in a worker's
  runtime: QuickJS-NG throws "cannot block in this thread", as only a runtime
  `JS_SetCanBlock` allowed blocks (vendor/rquickjs-sys/quickjs/quickjs.c `js_atomics_wait`;
  `JS_NewRuntime` callocs the runtime, and rquickjs 0.14.0 never sets it). The filter
  admitted a call whose resumption it refused; the fix closes that before a program,
  QuickJS-NG or musl comes to make one.
- **Fix.** `crates/server/src/sandbox.rs`: `restart_syscall` in `worker_calls`, which
  `helper_calls` extends; D136 records it.
- **Other filters of shards' own** (each checked, none changed). The VM process's
  (crates/shards/src/confine.rs) allows it already. The managed reaper's (init managed.rs:
  read, write, close, wait4, ppoll, rt_sigreturn, exit, exit_group) and the network
  process's (crates/net/src/confine.rs, x86_64 only) allow no call that resumes through
  `restart_syscall`: ppoll restarts as itself, wait4 restarts as itself (ERESTARTSYS), and
  epoll_wait returns EINTR (fs/eventpoll.c, signal(7)), which the network process's poller
  takes as no events (crates/net/src/poll.rs `interrupted_or`). `harness/probes/reaper.rs`
  runs the reaper's own loop (`reaping`) under its own signals and filter, with a child to
  wait for, stops and continues it in its wait, then ends its owner: it sleeps again after
  the stop and exits 125 as an owner's end makes it. `harness/probes/net.rs` runs the
  network process's poller under its filter, stopped and continued as it waits: it exits 0
  once its pipe is written. Both pass before any change (none was made): no defect to fix
  there (R09, R13). The filters init loads for a run, a build step and a domain, and the
  in-VM server's own, are Docker's default profile compiled (crates/seccomp), which allows
  `restart_syscall`, or stack on it (D121's mounts, D124's binds, the chooser: each allows
  what it does not name).

## Checks

Each row's log is in `logs/`, each harness in `harness/`: `checks.sh` runs the unit tests,
the probes and the mutants, a mutant's file put back and touched after it; `x86_64.sh` and
`arm64-docker.sh` run it on each host. A mutant is KILLED where its test fails.

| Where | Revision | What ran | Result | Log |
|---|---|---|---|---|
| x86_64 musl, CI's image | 883796e, fc71859 | init and server unit tests | 201 and 49 passed; 211 and 49 at fc71859 | x86_64-musl-* |
| x86_64 glibc, native | 883796e, fc71859 | the same | the same | x86_64-gnu-* |
| arm64 musl, CI's image | fc71859 | the same | 211 and 49 passed | aarch64-musl-fc71859 |
| arm64 glibc, Debian's image | fc71859 | the same | 211 and 49 passed | aarch64-gnu-fc71859 |
| each of the four | fc71859 | probe keep.rs on `changes::keep`, fixed / as main had it | O_PATH, the mode-0 layer kept / on musl neither: not O_PATH, EACCES; on glibc both pass | each |
| each of the four (net: x86_64) | fc71859 | probes reaper.rs, net.rs, atomics.rs | the reaper and the network process live through the stop; Atomics.wait throws "cannot block in this thread" | each |
| each of the four | fc71859 | mutant keep-through-std (a call site back on std) | KILLED by the source test | each |
| musl, both arches | fc71859 | mutant helper-as-std (std's flags in the helper) | KILLED: "not O_PATH" (x86_64 `0o300000`) | *-musl-* |
| glibc, both arches | fc71859 | the same | survives: glibc keeps O_PATH, which is why the source test exists | *-gnu-* |
| each of the four | fc71859 | mutant restart-helper-only (main's filters) | worker test KILLED (SIGSYS); helper test survives | each |
| x86_64, both libcs | fc71859 | mutant restart-none | worker and helper tests KILLED (SIGSYS) | x86_64-* |
| arm64, both libcs | fc71859 | the same | worker test KILLED; helper test survives (ppoll) | aarch64-* |
| x86_64, adacachy | fc71859 (main) | clippy -D warnings, init, server and net, all targets, glibc | clean | merged-fc71859-x86_64 |

Two arm64 runs were discarded, each the harness's fault: one ran both images on one copy
of the source, so that each image's probes and mutants changed the other's files mid-run
(its init unit tests met the other's `keep-through-std`); the next reused a target volume
that held a test binary built from a mutant by the run before, stopped mid-mutant, with the
source copied at older mtimes, so cargo reused it and the unit section's worker test ran
without `restart_syscall` (`logs/aarch64-gnu-fc71859-discarded-stale-build.log`). The run
kept gives each image a copy of its own, with fresh mtimes and fresh target volumes.

## Owed

- Ran at main 1464cdb88 on adacachy: `cargo fmt --all --check` clean, and clippy -D
  warnings of init, server and net, all targets, on the four Linux targets (x86_64 and
  aarch64, gnu and musl, scripts/lint's zig for their C) clean.
- To ROOT (not run here): scripts/lint's whole workspace, and its macOS and Windows
  targets, which compile none of this change (`cfg(target_os = "linux")`); real-VM runs of
  the guest paths that hold these descriptors (diff, commit, export and size of runs and
  joiners, D190's go-over) and of code mode under init's pause and commit.
