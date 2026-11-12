# main's CI failures of 2026-11-12: causes, fixes and their checks

Implementer lane `p/ci-green-arm64-v1`, from origin/main `7ad28fab136fb72cac910de9c0f4bfc1db2d81f9`.
POLICY_READ implementer: AGENTS.md, docs/development-rules.md (R01–R16, SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`) and
docs/development-evidence.md read in full from origin/main; CLAUDE.md. Coordinator: ROOT
(kiln agent 01a148884a4770fdaf0111f3f792a248). The person's rule of 2026-11-12 has
implementers land their own work on main once its unit and light checks pass.

CI: run 43230466376 (8f3cd5b74), jobs aarch64-unknown-linux-gnu 131224103882 and
aarch64-unknown-linux-musl 131224103787; run 43250570050 (7ad28fab1), aarch64-unknown-linux-gnu
131294914710; run 43189239973 (ac87190d3), x86_64-unknown-linux-gnu 131084056805 and
aarch64-unknown-linux-musl 131084056838; the firecracker comparison's jobs since 129515412706.

Environments: arm64 Linux is the Lima VM `g121-winarm` (Ubuntu, kernel 7.0.0-38, Docker
29.9.3); x86_64 Linux is adacachy (CachyOS, kernel 7.2.9, Docker); macOS is this Mac (macOS
26.4, arm64). The musl runs are CI's own: its pinned image
`rust:1.98.1-alpine@sha256:7cc1c22d…`, Docker's default seccomp profile, the tests as the
checkout's owner (`harness/musl-repro.sh`, after scripts/musl-ci).

## 1. shards-init's managed tests: clone3 refused in CI's musl container

- **Symptom.** `managed::tests::a_capture_failure_uses_the_same_owned_containment_and_keeps_its_cause`
  and `…a_real_observation_error_quarantines_only_the_owned_tenant`: `clone3: Function not
  implemented (os error 38)` at managed.rs:1070.
- **Cause.** Their fixture child (`Child::start`) is made by clone3. Docker's default profile
  answers clone3 ENOSYS without CAP_SYS_ADMIN (`crates/seccomp/data/default.json`, moby/profiles
  v0.2.3, `errnoRet: 38`) so that a libc falls back, and allows clone(2) without a namespace
  flag (mask `0x7E020000`). Seccomp 2 in the container; the same tests pass with
  `--security-opt seccomp=unconfined` (logs/arm64-musl-unconfined-before.log).
- **Fix.** The fixture forks as init's own forks do (`Launch::child`) and as glibc's fork does
  (sysdeps/unix/sysv/linux/arch-fork.h, glibc 2.42): clone(2), CLONE_PIDFD's descriptor at
  `parent_tid`, the third argument on x86_64 and on arm64's CLONE_BACKWARDS alike (Linux 6.18
  kernel/fork.c). Test code only.
- **No production fallback.** Every production clone3 of init's (domains.rs, build.rs, run.rs)
  asks CLONE_INTO_CGROUP, which clone(2) cannot carry: its flags are cut to 32 bits
  (`lower_32_bits(clone_flags)`, fork.c), and CLONE_NEWCGROUP's root is the cgroup the child is
  born in. glibc gives a cgroup birth no fallback either (spawni.c: ENOTSUP). A fallback no
  production request can take would be code for a test alone (R05, R13). On a kernel without
  clone3 (before 5.3) init's containment has no cgroup.kill (5.14) anyway, and PID 1 of a
  microVM runs under no seccomp filter.

## 2. shards-server's `an_unusable_helper_control_is_not_admitted`: musl's O_ACCMODE

- **Symptom.** `unusable_native_control_rejected=false … the native nonblocking setup error was
  discarded` (server.rs:2564), musl only, with or without seccomp.
- **Cause.** The fixture opened its O_PATH directory through `OpenOptions::custom_flags`. std
  masks custom flags with O_ACCMODE (library/std/src/sys/fs/unix.rs `open_c`, Rust 1.98.1), and
  musl's O_ACCMODE is `03|O_SEARCH`, O_SEARCH being O_PATH (include/fcntl.h, musl 1.2.5; libc
  0.2.189 `0o10000003`). On musl the descriptor was an ordinary directory's, whose F_SETFL
  succeeds; the product's refusal was never asked. `harness/opath.c` shows the kernel refusing
  F_SETFL on a real O_PATH descriptor (EBADF) inside the container and out.
- **Fix.** The fixture opens with open(2) itself and checks that its descriptor is O_PATH before
  it asks the product anything. Test code only.
- **Also found, not changed.** Four production opens of shards-init's (join.rs 915 and 1033,
  changes.rs 52, run.rs 3640), and a test's (join.rs 1398), ask `custom_flags(O_PATH |
  O_DIRECTORY)`, and shards-init is musl alone: they open O_RDONLY directories. Each is used
  only as a directory descriptor, by root; no defect is reproduced (R09), so they are left for
  the coordinator.

## 3. shards-server's `a_worker_not_reaped_ends_no_other`: the helper's clone3

- **Symptom.** `REAP_UNKNOWN first=[(2, -1, -1), (1, -1, -1)] … the other worker was ended
  with it` (sandbox.rs:1153), in the musl container.
- **Cause.** The code-mode helper made each worker by clone3; under Docker's profile every
  birth failed ENOSYS, and the helper said so, `-1, -1` being its creation refusal. Passes
  unconfined.
- **Fix (product).** The helper forks each worker by clone(2), which carries its request
  (CLONE_PIDFD, SIGCHLD) whole; its allowlist has `clone` in clone3's place, no wider. D136's
  helper "confines itself … to forking, and reaping and killing its workers by pidfd": unchanged.
- **A second cause, x86_64's: a stopped helper killed by its own filter.** Checking the fix
  natively on x86_64 glibc, the test failed once with `helper_signal=31`: the kernel logged the
  helper (`comm="shards-sandbox"`) killed for syscall 219, `restart_syscall`. poll(2), x86_64's
  (arm64 has only ppoll), always resumes through the restart block (Linux 6.18 fs/select.c
  `set_restart_fn(…, do_restart_poll)`), so a poll a stop broke into resumes by
  `restart_syscall`, which the helper's allowlist lacked. `harness/stop-helper.sh` stops and
  continues the helper as it waits: on x86_64, glibc and musl, the helper died 10 of 10 times
  each; on arm64 it lived 10 of 10. init stops a run's processes so in production: a cgroup's
  freeze for pause (D119, run.rs `freeze`) and around a commit's packing, else `kill(-1,
  SIGSTOP)`; D136 puts the helper in its agent's cgroup.
- **Fix (product).** The helper's allowlist has `restart_syscall`, as the VM process's
  (crates/shards/src/confine.rs) and Docker's default profile do. It calls only the pending
  `restart_block.fn`, else returns EINTR (kernel/signal.c): it resumes an allowed call, and allows
  no other. Workers wait in read(2), which a stop restarts as itself (ERESTARTSYS): stopped
  and continued so, they lived 10 of 10 on each host. Open, not reproduced: a worker in a timed
  futex wait (a program's `Atomics.wait` with a timeout, if QuickJS-NG blocks there) would
  resume by `restart_syscall`, which its own filter still refuses.

## 4. testguest's `udp_interception_parser_has_explicit_origins`: a stale test

- **Symptom.** `UDP local-loopback and local-self: the backend already holds the destination
  tuple … (EADDRINUSE)` (managed_network.rs:974), on arm64 glibc and musl alike.
- **Cause.** 961bba34c (finding 10) made the parser refuse those two UDP origins, measured in
  docs/research/measurements/managed-network-udp-local/coordinator-6e3d, and updated udp.rs's own
  test, not this one. A pure parser test: no architecture, kernel or container in it.
- **Fix.** The test holds `local` and `forward` accepted and the two refused with their reason;
  the probe's usage line no longer offers them for UDP.

The x86_64 jobs never reached items 1–4 (both time out in tests/api_image.rs first). The same
five failures reproduce on x86_64 musl (logs/x86_64-musl-before.log): none is arm64's.

## 5. The firecracker job's restore bench: "no working set kept"

- **Symptom.** crates/shards/benches/restore.rs:104 since d217bbbed (2026-11-09); the job
  passed at 0e6a9a7af. Every failing job's log says the same.
- **Cause.** da8bcd272 (D12) made the command line add `--console-said N` and `--said-to FD`
  right after `restore` for a VM that quotes its console (`--no-console`). The command line
  reads a restore's directory as the word after `restore` (`recording_restore`,
  `has_vm_flag`, and on macOS the template its grants broker is given). So `shards restore DIR
  --no-console` found `--said-to` where its directory was: on Linux and Windows no working-set
  writer started, and no working set was ever kept; on macOS the broker was given
  `<cwd>/--said-to` as the template (with the mutant's order, `<cwd>/--console-said`, seen on
  macOS: Checks), so the snapshot's files were opened as any path is, not beneath the template, never
  through a link (PM M165).
- **Fix (product).** `console_said` adds both options after every word it was given, `--said-to`
  with the word its caller's recorder gives (one place, Unix's and Windows'). A VM that quotes
  its console is given no `--` (said.rs `quotes`), so none goes to a workload.
- **No test caught it.** The E2E restores (tests/snapshot.rs) show their console, which takes
  no console options; only the bench restored with `--no-console`. `cli::said_options` now
  holds the placement.

## 6. The shipper test `a_part_refused_goes_again_in_its_lines_place`: gone peers

- **Symptom.** `Broken pipe (os error 32)` at gelf.rs:304 (aarch64 gnu, x86_64 gnu, x86_64
  musl); and, in 131084056838 (aarch64 musl), its third part's send unwrapped as
  `ConnectionAborted … sent again`.
- **Production handles it.** The shipper does every send and close through `until_done`
  (ship.rs), again after any error but FileTooLarge and the daemon's stop; the stream keeps what
  it holds when a write fails (`pump`, `close`). The test was narrower: it retried a close only
  on `ConnectionAborted`, and unwrapped its sends.
- **Fix (test).** `again_while_ended` (gelf.rs tests, was `close_until_ended`) does a send or a
  close again while the receiver has ended the connection: the stream's ConnectionAborted, or
  EPIPE, ECONNRESET (Linux 6.18 net/ipv4/tcp_input.c `tcp_reset`, net/unix/af_unix.c) and
  ENOTCONN once a reset has closed the socket (net/ipv4/af_inet.c `inet_shutdown`). Its sends go
  through it as its close does.

## Checks

Summary in docs/research/platform-measurements.md, M525. Each row's log is in `logs/`; each
harness in `harness/`. Code at 89c6a1da9 (merged with main at ddf154b18 for the lint).

| Where | What ran | Result | Log |
|---|---|---|---|
| arm64 Linux musl: Lima, the pinned rust:alpine, Docker's default seccomp | before: the five failing tests | each fails as on CI | arm64-musl-default-before |
| the same, `--security-opt seccomp=unconfined` | before | 1 and 3 pass; 2 and 4 fail | arm64-musl-unconfined-before |
| the same, default seccomp | after: shards-init, shards-server, shards-testguest unit tests | 197, 47, 2, 39 passed, none failed | arm64-musl-default-after |
| the same | after restart_syscall: shards-server; helper and workers stopped and continued | 47 passed; 10 of 10 | arm64-musl-server-restart-syscall |
| the same | four guard-removal mutants | all killed | arm64-musl-mutants |
| the same | shards: cli::said_options, cli::vm_flags | passed | arm64-musl-shards-after |
| the same | daemon::ship:: ×40 beside 6 busy loops: before; after, ×40 then ×100 | 38 of 40; 40 of 40, 100 of 100 | arm64-musl-ship-loaded-before, -shards-after, -ship-loaded-after-100 |
| the same | `shards restore DIR --no-console`, stand-in VM: fixed; the old placement | `--working-set` given, its writer opened DIR's generation; neither, and cli::said_options failed | arm64-musl-restore-args |
| arm64 Linux glibc: Lima, Debian `rust:1.98.1@sha256:a8a5f0a1…`, seccomp off, not root | after: the three crates; helper and workers stopped | 197, 47, 2, 39 passed; 10 of 10 | arm64-glibc-after |
| x86_64 Linux musl: adacachy, the pinned rust:alpine, default seccomp | before: the five | each fails as on arm64 | x86_64-musl-before |
| the same | after: the three crates | 197, 47, 2, 39 passed | x86_64-after |
| x86_64 Linux glibc: adacachy, natively | after: the three crates | 197, 2, 39 passed; shards-server: see below | x86_64-after, x86_64-server-restart-syscall |
| x86_64 Linux, glibc and musl | helper stopped and continued, before and after restart_syscall | 0 of 10 each; then 10 of 10, and with workers 10 of 10 | x86_64-stop-helper-audit |
| Windows x86_64: jupiter, a kiln agent, at 89c6a1da9 | shards: cli::said_options, cli::vm_flags, daemon::ship::; ship ×5 more | 32 passed, 1 ignored (a measurement); 30 passed each | the agent's report, logs not kept |
| macOS arm64: this Mac | scripts/lint: format, aarch64-apple-darwin | clean | — |
| Linux, four targets: adacachy | scripts/lint, at ddf154b18 | format clean; x86_64-unknown-linux-gnu still running at landing, the other three after it: owed to ROOT | — |
| macOS arm64: adas-mac-studio.local-2, a fresh clone at 89c6a1da9 | shards: cli::said_options, cli::vm_flags, daemon::ship:: | 31 passed, 1 ignored (a measurement) | macos-arm64-macstudio |
| the same | `shards restore DIR --no-console`, stand-in VM: fixed; the old placement | the broker given `--template DIR`; given `--template <cwd>/--console-said`, and cli::said_options failed | macos-arm64-macstudio |
| arm64 Linux musl: Lima | clippy -D warnings, all targets, of shards-init, shards-server, shards-testguest and shards, at ddf154b18 | clean | arm64-musl-clippy-changed-crates |
| Windows, two targets: this Mac | scripts/lint (cargo-xwin), at ddf154b18 | format and x86_64-pc-windows-msvc clean; aarch64-pc-windows-msvc still running at landing: owed to ROOT | — |

shards-server on x86_64 glibc: before restart_syscall it failed once in a full run,
`helper_signal=31`, the kernel logging the helper's syscall 219: restart_syscall follows only
an interruption no handler ran for (a stop, a freeze, a tracer), which something on that shared
host made; what is not known. harness/stop-helper.sh then gave it on demand. After both fixes it failed
once more, the first full run after its build at load 30, "the other worker was ended with
it": its REAP_UNKNOWN line was lost to that run's output filter, no seccomp kill was logged,
and the 66 full runs since (6, 40, and 20 beside 16 busy loops) passed. Its cause is unknown.

Not run here, owed to ROOT: the restore bench itself (the firecracker comparison job) and
every real-VM run (KVM, HVF, WHP), among them code mode in a guest under init's pause and
commit; any test on Windows arm64 (its lint only).
