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

## Owed to ROOT

- The 64+-concurrent exec E2E on KVM, HVF and WHP, fresh runs included; `daemon.log` should
  show no exec-port refusal while execs are pending.
- The relay's `exec_failures` flush and its cut and restore paths run only in a real guest.
- Found, not changed: the host's `to_guest` returns true when its send on the signal
  connection fails, and frames queue for ever once that connection is gone while the
  workload runs: an exec asked for then waits until the workload ends. Fixed since:
  [../signal-link/README.md](../signal-link/README.md).
