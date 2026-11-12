# The workload's signal connection: every frame sent, held or refused at once

ROOT's goal for session g121-46: the exec-admission lane found (its record,
[../exec-admission/README.md](../exec-admission/README.md), "Owed to ROOT") that the VM
process's `to_guest` returned true when its send on the guest's signal connection failed, and
that frames then queued for ever while the workload ran, so that an exec asked for after that
waited for the workload's end. Implementer lane `p/to-guest-v1`, from origin/main
`d35c4cf79`, origin/main `c2c4547c1` merged in before any edit. Policy
`docs/development-rules.md` SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, read in full first.
Owned paths: `crates/shards/src/workload.rs` and `workload/hellos.rs`, their callers in
`warm.rs` and `vm_run.rs`, the guest's end in `crates/init/src/run.rs`, `crates/abi/src/run.rs`'s
two constants, D16's text. Light tests only: real VMs are ROOT's.

## The defect, read and reproduced (R09)

`to_guest` at `d35c4cf79` (`crates/shards/src/workload.rs`): a connected link whose `send`
failed set the connection to none and answered `true`; with none and the workload running,
every later frame was pushed on an unbounded `Vec` for a dial that the port never takes
again (it takes one connection, AGENTFILE_ARCH.md §9.7). Three ways it bit, each reproduced
with real sockets and threads ([host-red-tests.patch](host-red-tests.patch) on `d35c4cf79`,
[logs/host-red-1.log](logs/host-red-1.log), adacachy, Linux 7.2.9 x86_64, rustc 1.98.1):

| Red test | Observed at `d35c4cf79` |
| --- | --- |
| `red_a_frame_the_guest_cannot_be_sent_is_told_it_went` | "the failed send told sent: true; later ones told sent: [true, true, true]; queued for ever: 3" |
| `red_an_exec_whose_frame_cannot_be_sent_waits` | "the exec still waits after 5 s: Err(Timeout)" |
| `red_an_exec_whose_frame_the_guest_never_read_waits` (the guest's end closes, the frame unread) | "the exec still waits after 5 s: Err(Timeout)" |

Read beside them: the managed run's dial thread, where a queued frame's send failed, kept
the connection and supervised nothing, its control boundary unheld; and the guest closes
its signal connection just before it sends its exit status on the other connection, so that
the host, had it read the close, could not have told a run's end from a loss.

## Decision (R02, R04, R05, R12, R13)

**An explicit link** (`workload::Link`): `Idle` (no workload, or its run over), `Awaiting`
the guest's dial (`Pending`), `Connected`, or `Lost`, each frame meeting it at once with a
typed answer (`Unsent`: `NotRunning`, `NoSuchSignal`, `Unheld`, `Lost(Lost)`; `Lost`:
`Closed`, `Protocol`, `Untaken`, `Failed { kind, os }`). Transitions: `will_run` Idle →
Awaiting; the dial (`dialed`) Awaiting → Connected, or Lost where a held frame's send fails;
a failed send Connected → Lost; the watch (`watch`): the guest's said end → Idle, anything
else → Lost; the run's end (`serve`) → Idle.

- **What waits for the dial is bounded by what is pending, never by time.** One frame for
  each thing pending, in the order they came, a later one in its place: each signal once, as
  Linux holds a standard signal pending once (Linux 6.18 kernel/signal.c l. 1037,
  `legacy_queue`); the terminal's latest size; an exec's start (one per exec waiting), and
  its signals and latest size only once its start waits; each ask's latest answer (D124).
  A signal number outside `1..=NSIG` (`_NSIG` 64: 6.18 include/uapi/asm-generic/signal.h
  l. 7, arch/x86/include/asm/signal.h l. 11) is refused: init drops it. Bound: 64 signals
  and a size for the workload and for each exec on its way, an answer for each ask. A
  real-time signal sent twice before the dial reaches its command once, which Linux would
  queue twice; dockerd drops every signal sent before its container starts (D16).
- **The guest says its end, last, on the signal connection** (`kind::SIGNALS_END`), as its
  managed control says its own (`MANAGED_CONTROL_END`), so the host tells a run's end from
  a loss on one connection, in order. Its connect, where the relay ended before seeing it
  finish, is waited for: the kernel ends a pending one within its connect timeout (6.18
  net/vmw_vsock/af_vsock.c l. 136, 1509, 1652) and refuses a send before (l. 2075).
- **The host watches the connection** (one thread, the dial's, blocked in a read: no poll,
  no wake until the end), so a loss is found as it happens, not at the next send: an exec
  whose frame went into a connection the guest closed would otherwise wait.
- **Lost is for good in its VM process.** The port takes one connection (§9.7); only a copy
  restored from a save dials again, in a VM process of its own, its frames held for that dial
  (D176). Alternatives: a redial in place, gated by a token as a restore's is (a protocol of
  its own, both sides, for a loss no run has shown: not built, R13); ending the run, as
  containerd ends a task whose shim connection closes (v2.4.2 core/runtime/v2/shim.go
  l. 146-197, `cleanupAfterDeadShim`; shim_manager.go l. 300, 375) because that close is the
  shim's death and the shim holds the task's stdio and exit: here init holds them and still
  says the command's end on the run connection, so the run goes on.
- **What a loss does.** Every frame is refused with the connection's own error; every exec
  still waiting for its connection fails with it (`hellos::fail_execs`: a restored run's and
  a capture's waits ask nothing there and wait on); the VM process's log, its daemon's, says
  it once. A SIGKILL its daemon sends, or its VM sends as the daemon goes, refused for a lost
  link, stops the machine (`warm::kill_command`, `vm_run::stop`): nothing else reaches the
  command. A stop signal refused as the daemon goes waits out no grace (moby
  docker-v29.8.1 daemon/stop.go l. 86-88 waits 2 s on a stop signal it could not send).
  `run --kernel` ends as a signal it cannot forward would end it.
- `exec_session` and `join` end at once with the typed refusal's words (`NotRunning` keeps
  "the container is not running").

## Live red and green (R09, R10)

Unit and light integration tests with real sockets and threads, in the VM process's and
shards-init's suites:

| Test | Red | Green |
| --- | --- | --- |
| `a_frame_the_guest_cannot_be_sent_fails_its_caller_and_every_later_one` | red 1 | ok |
| `an_exec_whose_frame_cannot_be_sent_ends_at_once_with_why` | red 2 | ok |
| `an_exec_whose_frame_the_guest_never_read_fails_as_its_connection_closes` | red 3 | ok (Linux: the close with the frame unread reads as a reset) |
| `a_guest_that_says_its_end_has_ended_its_run` (a send meeting the end first) | | ok |
| `a_signal_connection_is_watched_from_its_dial` | | ok |
| `a_signal_connection_saying_anything_else_is_lost` | | ok |
| `frames_wait_for_the_dial_one_for_each_thing_pending` | | ok |
| `an_exec_waiting_for_a_dial_that_fails_fails_with_it` | | ok |
| `only_execs_fail_as_the_signal_connection_goes` | | ok |
| init `the_relays_end_is_said_last_on_its_signal_connection` | | ok |

Linux x86_64 (adacachy, under its load of 70 to 79 on 32 cores),
[logs/linux-green.log](logs/linux-green.log) ([linux.sh](linux.sh)): shards-vm 57 of 57,
shards-init 207 (1 ignored, as before), the workload tests 200 of 200 runs, the guest's end
300 of 300.

Mutations, each a guard removed ([mutate.py](mutate.py), [mutations.sh](mutations.sh)),
all red for their reason, [logs/mutations.log](logs/mutations.log):

| Mutation | Fails |
| --- | --- |
| HM1 a failed send told it went | the frame, the exec (waits 5 s), the said end |
| HM2 a lost link takes frames | four: each later frame `Ok(())` |
| HM3 nothing reads the connection's end | five: execs wait, no loss found |
| HM4 the dial's connection taken unwatched | `a_signal_connection_is_watched_from_its_dial` |
| HM5 a loss fails no exec | the two execs wait |
| HM6 the said end taken for a loss | its exec told "lost", not "not running" |
| HM7 a said end keeps a loss | its exec waits |
| HM8 no frame in another's place | a signal sent three times went three times |
| HM9 an exec's signal waits without its exec | `Ok(())` for no exec |
| HM10 any number a signal | `Ok(())` for 65 |
| HM11 every wait fails with the link | a restored run's and a capture's too |
| GM1 the guest says no end | the host read nothing |
| GM2 the guest's pending connect not waited for | nothing said where the connect was pending |

## Portability (R11, R12)

- Shared code only: the link, its watch and the guest's end use the `platform` sockets and
  `shards_ipc::Polled`; no platform adapter added. No new syscall in the VM process's
  seccomp filter (a socket's dup and read, as `serve` already makes).
- Linux x86_64: above. `scripts/lint` for the four Linux targets, from adacachy (zig),
  [logs/lint-linux.log](logs/lint-linux.log) ([lint-linux.sh](lint-linux.sh)).
- macOS arm64 and the two Windows targets: `scripts/lint aarch64-apple-darwin
  x86_64-pc-windows-msvc aarch64-pc-windows-msvc` and the VM process's tests on the Mac
  Studio, [logs/mac.log](logs/mac.log) ([mac.sh](mac.sh)).
- Windows: no test of this change runs there (the workload's tests are Unix's); its build is
  the lint's. AF_UNIX on Windows resets a peer that writes after the other end closed, and
  it loses what it had not read (PM M358): a frame sent just as the guest says its end can
  make that end read as a loss there, said in the log as one; frames are refused and execs
  fail either way, at once.
- Not run: aarch64 Linux and Windows tests (no such host here), any real VM (ROOT's).

## Waits (R05)

- The link's lock is held across a send, as before: a guest that stops reading its signal
  connection (its vCPUs parked for a save, D176, or a pause) blocks every frame's caller
  until it reads again. Found, not changed: with a microVM paused (`shards pause`), an exec
  frame larger than the socket buffers and the vsock window blocks its exec's thread under
  the lock; the warm VM's daemon relay, which takes the lock for a SIGNAL (the daemon thaws
  a paused microVM as it sends SIGKILL), would then wait, unable to read the thaw behind it.
  Not reproduced (R09): owed, with a writer of its own or sends outside the lock its fix.
- The watch waits on the connection's end, a read, no deadline; execs wait on their
  connection, the guest's EXEC_FAILED, the watch's verdict, or the port's close; no new
  timer. `Pending` is no cache: frames waiting for one dial, each standing for one pending
  thing, gone as the dial takes them.
- Not covered: a guest that never dials (the host's muxer refusing it for want of a socket
  pair, `UnixStream::pair` in `serve`, or init unable to make its socket) says nothing, and
  what waits for the dial waits for the run's end, bounded as above. Telling the host needs a
  frame of init's on the run connection, held across a restore's cut as `exec_failures` are:
  owed, with its restore path in a real guest.

## Rules

| Rule | Evidence / gap |
| --- | --- |
| R01 | No unwrap, expect, panic or indexing in production; `try_reserve` for held frames (`Unheld`); poison-tolerant locks; clippy below. |
| R02 | containerd v2.4.2, moby docker-v29.8.1, Linux 6.18 sources cited above; alternatives (redial, end the run) weighed above. |
| R03 | `NSIG` is Linux's `_NSIG`; the 4-byte ids are the frames' u32s; tests: 5 s bounds an event that comes at once or never. No other literal. |
| R04 | `Link`, `Unsent`, `Lost` typed through callers; transitions above, each exercised. |
| R05 | Waits above; one owner of the link's state; the found lock wait owed. |
| R06, R07, R13 | Small functions (`hold`, `dialed`, `take_signals`, `watch`, `ended`, `lose`); comments state invariants and sources. |
| R08, R14 | No performance claim; the watch is one blocked thread per VM process, its cost unmeasured (ROOT's). |
| R09, R10 | Red, green, mutations above, logs kept. |
| R11, R12 | Above. |
| R15 | Implementer; merged to main by the person's 2026-11-12 rule; real VMs ROOT's. |
| R16 | Work dirs `~/g121-tmp/to-guest` on adacachy and `~/to-guest-target` on the Mac Studio: build output and copies, removed after. |

## Owed to ROOT

- Real runs on KVM, HVF and WHP: a run's normal end (the guest's said end; no "lost" line in
  `daemon.log`), a restore's redial (D176: frames held, then sent), execs at once, joiners'
  ends, a managed run's boundary; and a lost link provoked in a real guest (a malformed frame
  or a reset vsock stream), with `shards exec`, `kill` and a daemon's end seen to end at once.
- The paused-microVM lock wait above, and a guest that never dials.
- aarch64 Linux and Windows tests.
