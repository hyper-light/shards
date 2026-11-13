# Exec, attach and TTY into members (plan step 6, G24)

Kiln plan `01a1cd8e862e7169a074db3d5fa33f3d`, step 6: "exec a program inside a member, as its
uid and under its confinement; attach to a member's stdio; give a member a PTY that can be
resized". Role: implementer (replacing subagent aa2898462a4fd5e35, stopped by an
interruption). Policy read at origin/main e82b9403b: AGENTS.md `f66d304c…`,
docs/development-rules.md `df2acf47…`, docs/development-evidence.md `61613a24…`.

## Slice 1: a member starts with its signals at their defaults

### The defect (R09)

shards-init started a member with only its signal mask reset (domains.rs `exec_child`), and
execve(2) keeps an ignored signal ignored (signal(7), "Execve(2)"). Rust's runtime ignores
SIGPIPE in init before `main` (std `sigpipe::DEFAULT`), so every member, and everything it
started, began with SIGPIPE ignored: a member's process writing to a reader that had gone
saw EPIPE where one a shell starts dies of SIGPIPE. An exec's child already set every
disposition to SIG_DFL (run.rs `child`), as crun's exec does (src/libcrun/container.c
`unblock_signals`, crun 1.31 a84965e1); runc's children start at their defaults because
Go's runtime installs handlers, not SIG_IGN.

Reproduced live before the fix: `docs/research/measurements/member-exec-tty-v1/signals.sh
e82b9403b` on adalundhe-macbook-pro (Apple M5 Max, Darwin 25.5.0, Hypervisor.framework),
rev 90c3210f6 with this test, through kiln run r877 (logs `/tmp/s6-slice1b.7vPf` there):

| phase | domains.rs / defaults.rs | test | what the member said |
| --- | --- | --- | --- |
| red | main's (mask only) | FAILED | `confined signals ignored=0000000000001000 blocked=0000000000000000`, `confined pipe: exit 32` |
| green | `defaults::signals` | ok (1.62 s) | the two lines the test wants |
| mutant | `defaults::signals` without its disposition loop | FAILED | as red |

`ignored` is /proc's SigIgn bitmap recorded by the test guest's constructor, before its own
runtime ignores SIGPIPE: bit 12 is signal 13. `pipe: exit 32` is the guest's child, keeping
the disposition it was given, getting EPIPE; green's `signal 13` is that child killed.

shards-init's unit test `defaults::tests::signals_are_a_new_processs_whatever_its_parent_ignored_or_blocked`
(a child of a parent that ignores SIGUSR2 and blocks SIGUSR1): on adacachy (CachyOS
7.2.9, x86_64), at 38008233f with this change, the whole suite 229 passed; with the loop
removed it fails, "2: a signal is still ignored".

### The fix

`defaults::signals` sets every disposition to SIG_DFL (`1..=NSIG`, the kernel's `_NSIG`,
64 on both guest architectures; SIGKILL, SIGSTOP and musl's own 32-34 refuse and keep
theirs), then empties the mask, and is async-signal-safe for a child of `clone`. A member's
start and an exec's child both call it; the exec child's old loop stopped one short of
SIGRTMAX.

### Run it (ROOT's legs)

    cargo test -p shards --profile test-release --test member_exec

Owed, ROOT's: the test on KVM x86_64 (adacachy, adacachy2), WHP (jupiter), and arm64 Linux
KVM through the nested L1 path (boot.rs); shards-init's unit test on arm64 Linux.

### A finding outside this slice: members' output is not the run's stderr anymore

The test as first written read `[agent main] confined …` from a daemon run's stderr, as
tests/build.rs's `confined` test does. On the M5 Max at 90c3210f6 (kiln run r740) red, green
and mutant all failed the same way: the run's stderr held only its `shards-timing` line, no
`[agent ` line in any log, and the daemon's log said `managed output capture: managed output
closed`. A member's stdout and stderr go to its owner's endpoint (D180, init capture.rs:
"Private managed-member output"), never to the run's own stderr, on every host. So the
tests that read members' lines from a daemon run's stderr (build.rs's `confined` and its
kin) cannot pass on any host; ROOT's queue lists them as failing on the CachyOS KVM hosts
alone. This slice's test reads the harness's stdout as its owner does, through shards-api
(`ImageShards::boot`, `take_output`).

### R01–R16, slice 1

- R01: `defaults::signals` returns whether the mask was set; no panic path (no indexing,
  no unwrap); the test guest's probe is test code.
- R02: crun's and runc's behavior read at their sources (crun 1.31, runc v1.5.1
  8f2685a4); signal(7), execve(2), pid_namespaces(7).
- R03: the one bound is the kernel's `_NSIG` (`shards_abi::run::NSIG`, cited there).
- R04: no new state; the member's contract (domains.rs module doc) says it.
- R05/R06/R13: one function shared by both call sites, replacing two loops.
- R07: comments carry the execve and runtime facts the code cannot.
- R08/R14: no performance claim; 64 signal(2) calls per start, unmeasured here.
- R09/R10: above, red, green and the guard removed, live and in the unit test.
- R11/R12: guest code, the same for every host; HVF run here, the rest owed above.
- R15: implementer's light runs only; the real-VM suites are ROOT's.
- R16: nothing reclaimed.
