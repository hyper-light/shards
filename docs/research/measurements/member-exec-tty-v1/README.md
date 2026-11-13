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

## Slice 2: a command run in a member (D243)

`shards exec vm --for NAME VM CMD [ARG...]` runs CMD in the agent or harness NAME of the
microVM's image, confined exactly as that member is. The daemon resolves NAME to the member's
declared index and sends the run protocol's existing exec with one setup entry,
`in-member=N`. Init (`enter.rs`) is born into the member's PID namespace and its cgroup
(clone3 `CLONE_INTO_CGROUP`, held to its `pids.max`), enters its other namespaces from a
pidfd of its command, and confines itself through `domains::confine` with the member's kept
`Confinement`, the same function the member's own first process runs. Its stdio is pipes,
or a pty whose peer comes from TIOCGPTPEER, given to the member's uid, resized by the
existing `EXEC_RESIZE`. What stops it is typed: `members::Refusal` under
`exec_failed::MEMBER`, said with `docker exec`'s statuses. Design, alternatives and literals:
D243.

### Light results

At 634d7f22d, this slice merged with main (the person's rule of 2026-11-13: focused tests,
init's on Linux, where alone it runs, the rest on one host; ROOT's host rule moved them off
adacachy). An earlier round at bb88be4c8 on adacachy, before the last merge, passed the same.

| host | what ran | result |
| --- | --- | --- |
| Linux x86_64 (adacachy2, rustc 1.98.1) | `cargo clippy -p shards-init --all-targets -- -D warnings` | clean |
| the same | `cargo test -p shards-init` | 235 passed, 1 ignored (the five `enter::tests` among them) |
| macOS arm64 (adas-mac-studio-3, M4 Max) | `cargo fmt --all --check` | clean |
| the same | `cargo clippy -p shards-abi -p shards-ipc -p shards-cmdline -p shards --all-targets -- -D warnings` | clean (tests/member_exec.rs's real-VM tests among what it compiled) |
| the same | `cargo test -p shards-abi -p shards-ipc -p shards-cmdline` | all pass (abi's `members::tests` among them) |
| the same | `cargo test -p shards --bin shards -- devices:: spec:: commands:: workload::` | 22 passed (`a_member_exec_takes_nothing_its_member_has_not`, `a_member_is_found_by_its_name`, `a_member_commands_variables_go_as_given_for_init_to_lay_over_its_members` among them) |

### Guards removed (R10)

Each guard this slice adds, removed alone, fails the unit test named; each test passes with
it in place (adacachy, against this slice's code before its commits were squashed).

| | guard | removed by | test that fails |
| --- | --- | --- | --- |
| M1 | the daemon refuses `--privileged` | `if exec.privileged` made false | `daemon::commands::tests::a_member_exec_takes_nothing_its_member_has_not` |
| M2 | the daemon refuses `--user` | `if !exec.user.is_empty()` made false | the same |
| M3 | none of the microVM's variables under a member's command's | `if o.member` made false | `spec::tests::a_member_commands_variables_go_as_given_for_init_to_lay_over_its_members` |
| M4 | one member for one exec | the second `in-member=` let through | `run::members::tests::an_exec_names_one_member` |
| M5 | no exec names `SHARDS_DOMAIN` | its comparison taken out of `over` | `enter::tests::an_exec_cannot_set_what_init_sets_or_nothing` |
| M6 | init refuses a user of the command's own | `if !spec.user.is_empty()` made false | `enter::tests::a_member_command_takes_nothing_its_member_has_not` |
| M7 | init refuses any other setup entry | the `EXEC_KEY` check made false | the same |
| M8 | a stdio end's owner, or why not | `own`'s fchown result ignored | `enter::tests::a_stdio_end_given_to_another_uid_says_whether_it_was` |
| M9 | a terminal's peer's owner, or why not | `terminal`'s fchown result ignored | `enter::tests::a_terminal_is_of_its_size_and_its_peer_the_members` |
| M10 | a terminal's size as asked | its TIOCSWINSZ skipped | the same |

M8 and M9 hold as the tests run, as a user other than root (euid 1000 on adacachy): fchown(2)
to another uid is refused EPERM, and the functions say so. Run as root, the tests take the
other arm, where fchown succeeds.

### Run it (ROOT's legs)

    cargo test -p shards --profile test-release --test member_exec

`a_command_run_in_a_member_is_confined_as_the_member_is` (its uid and gid and no group, its
member's PID namespace, name, capabilities and `no_new_privs`, signals, Landlock on files and
TCP, seccomp, cgroup and network; another member's files by path, by symlink and by hard
link refused; nothing of init's held; the other member entered as itself),
`what_a_member_has_not_is_refused_to_a_command_run_in_it` (no such member, `-u`,
`--privileged`, a program its `PATH` does not hold) and
`a_command_run_in_a_member_has_a_terminal_that_resizes`, beside slice 1's
`a_member_starts_with_every_signal_at_its_default`. Owed, ROOT's: all four on KVM x86_64,
KVM arm64 (the nested L1 path), HVF and WHP; shards-init's unit tests on arm64 Linux.

### R01–R16, slice 2

- R01: no panic path in production code: errors returned and typed (`enter::Failed`,
  `members::Refusal`, `devices::NoMember`, `commands::MemberExec`); slices by `get`, the
  child's report by pattern; clippy's denied lints on the host below.
- R02: runc v1.5.1 and crun 1.31 read at their sources for joining a container; Docker's
  exec resize (moby, containerd); setns(2), clone(2), cgroup-v2's PID controller,
  landlock(7), TIOCGPTPEER(2const), pid_namespaces(7) (D243, and kiln research
  01a1d2be234e7711a389f3e1d52731db).
- R03: no chosen number: wire codes and statuses, each with its source in D243's Literals.
- R04: refusals typed end to end, each with its cause; no new lifecycle state, the member's
  own ending ends the command (pid_namespaces(7)).
- R05/R12/R13: the member's start and a command in it share `confine` and `Confinement`, a
  command shares `born_in` with the run's execs, and the exec takes the run protocol's
  existing frames; no new channel or manager.
- R06/R07: `enter::start` refuses in order, then one child that execs or reports a step;
  comments carry the kernel's and the runtimes' facts.
- R08/R14: no performance claim; a command in a member costs one clone3, one setns and the
  member's confinement steps, unmeasured here.
- R09: a feature, not a defect: no defect to reproduce.
- R10: above, every guard removed fails its test; the real-VM tests are ROOT's.
- R11: init's code is Linux's, the same in every guest. Its tests and clippy ran on Linux
  x86_64, the host's code's on macOS arm64, as the person's rule of 2026-11-13 has it; CI on
  main runs the eight-target matrix, ROOT's to watch; the real-VM legs owed above.
- R15: implementer's light runs only.
- R16: build output under `~/g121-tmp/s6-exec-tty` (adacachy, removed), `~/g121-tmp/s6-member-exec`
  (adacachy2) and `~/s6-member-exec` (adas-mac-studio-3), removed when the step ends.
