# Exec connections: admitted under contention, and said when lost

ROOT's goal "Shards exec hang root-caused and fixed" (session g121-46), implementer lane
`p/exec-admission-v1`. Base origin/main `451672250`; policy `docs/development-rules.md`
SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, read in full
before any edit. Owned paths: `crates/shards/src/workload/hellos.rs`, the exec parts of
`crates/shards/src/workload.rs` and `workload/capture.rs`, `crates/init/src/run.rs`'s exec
relay, `crates/vmm` `Admission`. Light tests only: the 64+-concurrent E2E on KVM, HVF and
WHP is ROOT's.

## The symptom and its causes

ROOT's record ([r11](../ring-pins-v1/logs/r11-exec-hang-64-at-once.txt), adacachy KVM):
of 64 concurrent `shards exec` into one run, two CLIs never ended, the daemon saying
nothing. Read in the code at `451672250`, and each reproduced below:

1. **Admission refused on a busy lock.** The exec port's admission, on the vsock device
   thread, took the listener's lock and then the waiters' lock (`PENDING`) with `try_lock`,
   and answered `Full` when either was merely held: by an exec thread registering, by the
   acceptor, by `heard`. The muxer then reset the guest's connect, said at debug only.
2. **An admitted connection went with another exec's wait.** An admitted connection was
   parked in the first waiter's slot without one, whoever's it was; that waiter's removal
   (its HELLO heard, its `EXEC_FAILED`, a forgotten wait) dropped the slot, and with it the
   other exec's connection.
3. **The guest dropped such losses silently.** In `crates/init/src/run.rs`, an exec whose
   connect failed, whose write failed, or which read its end before the host had it, set its
   connection to `None` and said nothing: no `EXEC_FAILED` reached the host, whose
   `exec_session` waited in `rx.recv()` until the workload ended, holding its client.
4. Lock order: registration took the waiters' lock, then the listener's; admission the
   listener's, then the waiters'. Made blocking as it stood, admission would deadlock.

## Decision (R02, R05, R12)

**Host: admission is the device thread's hand-off, under a lock of its own** (`hellos.rs`).

- `Door` (the wake socket's write end and the connections admitted) is a leaf lock held only
  to push, take or close, never across a wait, no other lock taken under it. The device
  thread waits on it no longer than another holder's push or take, and on nothing that
  waits for the device thread: no deadlock is possible (every holder: `Door::admit`, the
  acceptor's take, `close`, `exec_port`, which nests WAITERS then DOOR; nothing takes
  WAITERS under DOOR).
- `Waiters` (the listener's state and what waits, by token) is never taken by the device
  thread. How many wait is an atomic stored under its lock. A registration precedes the
  `EXEC` frame that asks the guest for its connection, and the device thread carries that
  frame to the guest before the guest can dial, so admission reads it before that
  connection arrives.
- Refused only where the port has closed, nothing waits, or no memory holds it; each
  reason's first refusal at `warn` in the VM process's log, every one at debug (a workload
  may dial the port at will). `Admission::Full` is gone: nothing refuses for capacity.
- Admitted connections are their own list, no waiter's until a HELLO says whose: a waiter's
  end takes no other's connection.
- Held candidates are bounded as before by what waits, every one read first: the silent
  longest goes (it was the last one pushed), the count taken before the reads less what they
  hand over, so that a waiter gone mid-round costs no other its own. Every waiter's
  connection has its waiter, so past their number at least one candidate is no waiter's.
- Matching is by token for any `Target` kind: execs, restored runs (D176), captures, and the
  member-stdin connections ROOT's step 5 adds (D231 v1 §3.3).

Alternatives: one blocking lock over waiters and arrivals (the device thread then waits
behind registration's duplicate scan and allocation, and a proof of the lock order with
`heard`); an `mpsc` channel as the engine port has (lock-free, but `ServedPort::Admitted` is
a function pointer: a channel needs a static sender no test can reopen, or a forwarding
thread per VM process as `accept_hellos` has, a thread more in every VM, PM M34).

**Guest: an exec's connection lost before the host spoke on it is said, and its process
ended** (`run.rs`, `Exec::on_conn`, `Exec::lose`).

- Explicit `Link` states (R04): `Dialing → Unheard → Heard`, or `Gone`. The host says
  nothing on an exec's connection before it has it (its first STDIN frame, or a joiner's
  SAVE), so a loss while `Dialing` or `Unheard` may be one the host never had: the host is
  told `EXEC_FAILED` with the exec's id and why, and the exec's process, which no client will
  hear, is killed.
- Docker's semantics (moby docker-v29.8.1 `daemon/exec.go`, `ContainerExecStart`): the
  streams are attached before `tsk.Exec` starts the process, and on the request context's
  end the daemon sends the process SIGKILL ("Sending KILL signal to container process").
  shards starts the process as its connection is dialed, not once it is up, and kills it on
  such a loss: waiting would put the dial's round trip on every exec's start (the start path's
  budget, architecture §4: 5 ms in all, the guest's command start 2.5–3.5 ms of it), for a
  loss that, with the host's fix, comes only where the port has closed or a workload's own
  dials crowd the held candidates. That round trip is unmeasured here (ROOT's).
- An exec that has said all it has keeps its connection until the host speaks on it or lets
  go of it: closed first, a host that never had it would not hear (the counterpart of the
  host's bound). For an interactive exec whose client never wrote, the host's close after
  EXIT reads as such a loss, and the host ignores the `EXEC_FAILED` it brings (no waiter has
  its id).
- A restore (D176): an exec of the microVM saved, whose connection the restore reset, is let
  go of unsaid and unkilled (its `born` generation, the microVM's `generation()`), and the
  relay's `exec_failures`, held while a cut is not open and cleared as the restore is found,
  carry no failure to the restored microVM's host, which gives its own execs the same ids.
  `Exec::start`'s dial failures take the same path.
- Host: what the guest says past its command's end (`beyond`) takes `EXEC_FAILED`, for the
  execs of joiners that outlive it (D119); it was a protocol error during a layer save.

## Live red and green (R09, R10)

Unit and light integration tests with real sockets, threads and processes, on adacachy
(Linux 7.2.9-2-cachyos x86_64, rustc 1.98.1), each command in its log.

| Check | Red (`451672250` behaviour) | Green |
| --- | --- | --- |
| `admission_takes_a_connection_while_the_waiters_lock_is_held` (and the listener's) | `Full` both, [host-red-1](logs/host-red-1.log) | ok, [host-green-1](logs/host-green-1.log) |
| `an_admitted_connection_outlives_another_execs_wait` | `Err(Timeout)`: lost | ok |
| `execs_at_once_each_get_their_own_connection` (64 at once) | 38 of 50 runs failed: 88 execs refused `Full`, 39 admitted and lost, of 3200, [host-red-repeat](logs/host-red-repeat.log) | 200 of 200, [host-green-repeat](logs/host-green-repeat.log) |
| guest `exec_link_tests` (connect refused, write and read losses, restore reset, waits to hear) | 4 of 5 red on the unchanged logic moved into `on_conn`, [init-red-1](logs/init-red-1.log) | 6 of 6; 500 of 500 repeated, whole shards-init 20 of 20, [init-green-3](logs/init-green-3.log) |
| whole shards-vm, shards-init, shards-vmm vsock | | 48, 203 (1 ignored), 66; [final-linux](logs/final-linux.log) |

The red host tests are [host-red-tests.patch](host-red-tests.patch) on `451672250`. The guest
tests' first runs flaked 7 in 300 ([init-green-2](logs/init-green-2.log)): a child another
test forked held each closed peer until it exec'd; each test now waits until its peer's end
shows (`ended`), and holds its refused port bound (no other process, nor the connect itself,
can take it).

Mutations, each of a guard removed ([mutate.py](mutate.py), [mutations.sh](mutations.sh)),
all red for their reason, [mutations-2](logs/mutations-2.log) (the header's rustc is the
directory's default; the runs used the tree's pinned 1.98.1):

| Mutation | Fails |
| --- | --- |
| HM1 admission refuses when the waiters' lock is busy | the busy-lock test (`Closed`), and the 64 at once |
| HM2 admitted connections dropped as a waiter fails | `an_admitted_connection_outlives_another_execs_wait` |
| HM3 the newest candidate evicted | `past_the_waiters_the_one_silent_longest_goes`, `exec_connections_go_only_to_the_exec_they_name` |
| GM1 no kill | connect and loss tests (SIGTERM, not SIGKILL) |
| GM2 nothing said | the three loss tests |
| GM3 no generation guard | `an_exec_a_restore_reset_is_let_go_of_unsaid` |
| GM4 an ended exec lets go unheard | `an_ended_exec_waits_to_hear_from_the_host` |
| GM5 the guest fix reverted whole | 5 of 6 |

## Portability (R11, R12)

- Clippy `-D warnings`, `--all-targets`, of shards-vmm, shards-vm-process and shards-init:
  the six Linux and Windows targets from adacachy, [lint-final](logs/lint-final.log), and
  again with origin/main `ad600dd52` merged in, [lint-merged](logs/lint-merged.log);
  aarch64-apple-darwin on the Mac Studio, [macos-tests](logs/macos-tests.log) (shards-vmm's
  build compiles C for macOS, so not from Linux). The first pass found `reopen` and `TESTS`
  dead on Windows, [lint](logs/lint.log): the hellos tests now run everywhere.
- Linux x86_64: everything above; the merged tree's suites and 100 repeats,
  [merged-linux](logs/merged-linux.log).
- Windows 11 (10.0.26200, adacachy's guest, x86_64-pc-windows-msvc): the hellos tests and
  shards-vmm's vsock tests, [windows-tests](logs/windows-tests.log). The workload tests are
  Unix-only, as before.
- macOS 26.4 arm64 (Mac Studio, `2944d9ab5`): the workload tests (23, every exec-port test
  among them) and shards-vmm's vsock tests (66), [macos-tests](logs/macos-tests.log).
- Not run: aarch64 Linux and aarch64 Windows tests (no such host here), the workspace-wide
  `scripts/lint`, any real VM (ROOT's).

## Member stdin connections (plan step 5)

Built on this admission by `p/s5-stdio-v1` ([member-stdin-v1](../member-stdin-v1/README.md)):

- **Host.** Each member whose owner writes its stdin has a token of its own, which the VM
  process registers before the run's setup goes to the guest (`register(token,
  Target::Input { member, to })`, workload/capture.rs `start_inputs`); a registration whose
  setup never goes is dropped with `hellos::forget(&token)`. A HELLO saying it delivers the
  connection to the input's hand-over thread (`Target::deliver`'s `Input` arm), which passes
  it to the run's owner (`MANAGED_INPUT`) as an engine connection goes.
- **Guest.** Init's input relay has explicit states of its own (`Dialing → Hello →
  Relaying`, each into `Ended`; crates/init `input.rs`), as an exec's `Link` has. The host
  says nothing on an input connection before its owner's first bytes, so a loss there is
  never silent and never waited on: a connect that fails (`connect_result`), a hello that
  cannot go, a connection that ends or fails before the owner's end, each closes the
  member's stdin, so the member reads its end, and is told its owner typed
  (`MANAGED_INPUT_ENDED`: the bytes its stdin took and the errno, `ECONNRESET` for an end
  without the owner's) on the managed control connection, whose room for it is reserved as
  the run starts. The member is not killed, as an exec's process is: an exec's stdio has no
  other reader once its connection is lost, while a member's stdout, stderr and life stay
  its owner's (P3: a lost stream ends that stream, not the member).

## Rules

| Rule | Evidence / gap |
| --- | --- |
| R01 | No unwrap, expect or indexing in production; locks poison-tolerant; `saturating_add` on the frame length; clippy clean. |
| R02 | moby docker-v29.8.1 `daemon/exec.go`; alternatives above. The dial round trip saved is unmeasured (ROOT's). |
| R03 | No new production literal. Tests: 64 is ROOT's count; 5 s bounds a positive event. |
| R04 | `Link`, `Listener`, `Refusal`, `Door::wake`; typed `Admission`. |
| R05 | Waits: admission waits on `Door` only; the acceptor on readiness (wake socket, candidates); `exec_session` ends on its connection, `EXEC_FAILED` or the port's close. `WAITING` mirrors the waiters' count, stored under their lock at every change: no cache. |
| R06, R07, R13 | Admission, matching and the acceptor's round are separate functions; comments state the lock and generation invariants. |
| R08, R14 | No performance claim; no profiling done (ROOT's). |
| R09, R10 | Red, green and mutations above, logs kept. |
| R11, R12 | Above; shared `platform` sockets, no platform adapter added. |
| R15 | Implementer; merged to main by the person's 2026-11-12 rule; heavy E2E ROOT's. |
| R16 | Work dirs `~/g121-tmp/xa-exec` on adacachy, `C:\xa-exec` on the Windows guest, `~/xa-exec-target` on the Mac Studio: build output only, removed after. |

## E2E on real microVMs (lane `p/exec-e2e-v1`)

ROOT's E2E goal (session g121-a0), implementer lane `p/exec-e2e-v1` from origin/main
`34215b23e`; policy SHA-256 as above, read in full first. Owned paths:
`crates/shards/tests/containers.rs` (the test), [e2e.sh](e2e.sh), [variant.sh](variant.sh),
`logs/e2e-*`, this section.

`execs_at_once_each_end_with_their_commands_status`, beside `exec_runs_commands_in_a_running_microvm`
(containers.rs runs boot cold; no pool is in the exec path):

- **How many:** twice the microVM's vCPUs, and at least 64: r11's case, 64 at once into a
  microVM of 32 (its pins test's twice the vCPUs), and a microVM has as many vCPUs as its host
  has CPUs (D191). 64 on every host here and on CI's.
- **What each runs:** `/bin/testguest MODE`, MODE its own (`up-17`) and no mode of the test
  guest's: it says `unknown mode "MODE"` on stderr and exits 2 at once, as r11's commands
  failed at once on stderr. Each client's output is its own, so a connection given to another
  exec's wait, or a loss said as a failure, shows as surely as a hang.
- **Where:** into a run that is up (its command said `ready`, and a lone exec ran), and into
  one coming up: its execs begin once the daemon has it (`inspect` finds it), before its
  microVM is up, so each waits for the start in the daemon (`await_start`) and they reach its
  guest together. Every burst into a fresh run here began so (`begun as it was created`).
- **What must hold:** each client ends with status 2, its own `unknown mode` line on stderr and
  nothing on stdout; the daemon's log says no `the exec port refused a guest's connection`
  (each reason's first refusal is said at warn).
- **Patience, no deadline:** the run's own pace is a lone exec of the same kind into the same
  run, spawn to exit. Served one at a time at that pace, a burst of n would end within n
  paces: a client still waiting once n paces have passed with none ending, counted from the
  burst (or the fresh run's start) and again from each that ends, fails the test, naming each
  with what it said, unless the daemon or its microVM still progresses (the suite's
  `Patience`). Under 2 and 4 times the host's CPUs of busy loops the lone exec slowed with the
  bursts (6.7–10.7 ms, 12.8–21.9 ms) and every round passed, the slowest whole burst taking
  0.69 of its n paces ([e2e-linux-g-load](logs/e2e-linux-g-load.log)); n paces bound the gap
  between two ends, each far shorter. Past n paces `Patience` sees the daemon's log once a second
  and a microVM's CPU each 10 s, so on a host stalled longer than n paces with nothing
  ending it may fail first; none did here.

### Red and green (R09, R10)

[variant.sh](variant.sh) builds each tree in a target directory of its own and says whether the
VM process it built carries the host fix (`the exec port refused a guest's connection`) and the
init the guest fix (`the command's connection to the host failed`); then [e2e.sh](e2e.sh)'s 10
rounds. g is `c16facdb2` (Linux) and `fecd60256` (macOS, the same test with e2e.sh); r0, rh and
rg are it with `2944d9ab5` and `0d4f20288`, `0d4f20288` alone, and `2944d9ab5` alone reverted.

| Variant (fixes it carries) | Linux KVM x86_64, adacachy2 (7.3.3-1-cachyos, 32 CPUs) | macOS HVF arm64, Mac Studio (Darwin 25.4.0, 32 CPUs, load 18–32) |
| --- | --- | --- |
| g (host, guest) | 10 of 10; bursts 46–78 ms, a lone exec 4.3–6.5 ms, [e2e-linux-g](logs/e2e-linux-g.log) | 10 of 10; bursts 52–114 ms, a lone exec 5.9–24 ms, [e2e-macos-g](logs/e2e-macos-g.log) |
| r0 (neither) | **0 of 10**: 1–3 of 64 clients never ended, nothing on their stderr, in the burst into the run up 6 times and into the fresh run 4, [e2e-linux-r0](logs/e2e-linux-r0.log) | **4 of 10**: 6 rounds left one client of the up burst that never ended, [e2e-macos-r0](logs/e2e-macos-r0.log) |
| rh (guest) | **0 of 10**: 1–3 clients ended 1 with `shards: the command's connection to the host failed before the host had it: the host closed it`, [e2e-linux-rh](logs/e2e-linux-rh.log) | **3 of 10**: 7 rounds, 1–3 clients ended so (`; the command was killed` where it still ran), [e2e-macos-rh](logs/e2e-macos-rh.log) |
| rg (host) | 10 of 10, [e2e-linux-rg](logs/e2e-linux-rg.log) | 10 of 10, [e2e-macos-rg](logs/e2e-macos-rg.log) |
| m: merged with origin/main `99d88fccf` (`34329a8ab`) | 10 of 10, [e2e-linux-m](logs/e2e-linux-m.log) | 10 of 10, [e2e-macos-m](logs/e2e-macos-m.log) |

The hang is the host's admission (r0); the guest fix turns the host's refusals and losses into
failures said (rh), which the test also fails on, not as its commands ended. With the host fix
nothing is refused or lost here, so the guest fix's own paths are not reached by this test
(rg passes); its unit tests hold them (GM1–GM5). No refusal was said at warn in any green round.

A trap on the way: the first red builds shared the green build's target directory, and
macOS's carried the green VM process (its string present, one helpers digest) and passed 10
of 10. The helpers' nested build found one `shards_vm` unit for both trees, built at 14:38:39,
and the red tree's files, checked out at 14:36:47 before the green build, older than it:
nothing was rebuilt. Each variant now has its own target directory, and variant.sh shows what
its binaries carry.

Portability (R11): Linux KVM x86_64 and macOS HVF arm64 above. WHP x86_64: CI's Windows jobs
(windows-2022 and windows-2025, `SHARDS_REQUIRE_VMS=1`) on main: run
[43328666195](https://github.com/hyper-light/shards/actions/runs/43328666195), of the push that
landed it (`220fb2be6`), queued then; CI's x86_64 Linux job too. Not run: Linux arm64 KVM (no host here but L1 on an M3 Mac), WHP
arm64 (no host). `scripts/lint`, all seven targets, of `34329a8ab` on the Mac Studio: clean, [e2e-lint](logs/e2e-lint.log).

| Rule | E2E lane: evidence / gap |
| --- | --- |
| R01 | Test code only. |
| R02, R12 | The suite's own conventions (`home`, `start`, `Patience`, `stacks_of`, `common::command`); no platform code. |
| R03 | 64 and twice the vCPUs are r11's case; the patience is n lone paces, past them the suite's `Patience`; the 5 ms look is the file's own (`exit`). |
| R04, R05, R06, R07, R13 | Two helpers (`execs_at_once`, `each_ends`). The clients' ends are looked at every 5 ms, as the file's `exit` looks: an ended child stays until it is reaped, so no look misses one. Comments give the derivations. |
| R08, R14 | No performance claim; the times above are the test's own output. |
| R09, R10 | Red, green and the partial reverts above, logs kept; the harnesses committed. |
| R11 | Above; WHP from CI, arm64 Linux and Windows unverified. |
| R15 | Implementer; merged to main by the person's 2026-11-12 rule. |
| R16 | `~/g121-tmp/exec-e2e` on adacachy2 and the Mac Studio, removed after. adacachy2's disk was full (2 GB free): removed `~/fc-chk-target`, `~/slates-gate-target` and `~/fc-target` (15.8 GB, CACHEDIR-tagged cargo target directories, no file newer than three days); kept `~/slates-gate-target-b4` and `-main`, which had newer files. |

## Owed to ROOT

- WHP's E2E result: CI run 43328666195's Windows jobs on main (above), queued at landing.
- The relay's `exec_failures` flush and its cut and restore paths run only in a real guest.
- Found, not changed: the host's `to_guest` returns true when its send on the signal
  connection fails, and frames queue for ever once that connection is gone while the
  workload runs: an exec asked for then waits until the workload ends. Fixed since:
  [../signal-link/README.md](../signal-link/README.md).
