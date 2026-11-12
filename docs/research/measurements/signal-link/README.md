# The workload's signal connection: every frame sent, held or refused at once

ROOT's goal for session g121-46: the exec-admission lane found (its record,
[../exec-admission/README.md](../exec-admission/README.md), "Owed to ROOT") that the VM
process's `to_guest` returned true when its send on the guest's signal connection failed, and
that frames then queued for ever while the workload ran, so that an exec asked for after that
waited for the workload's end. Implementer lane `p/to-guest-v1`, from origin/main
`d35c4cf79`, origin/main `c2c4547c1` merged in before any edit and `5564278e3` before the
final checks (`f5bfab285`). Policy
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

At the branch's merged revision `f5bfab285` (origin/main `5564278e3` in), Linux x86_64
(adacachy, under its load of 51 to 85 on 32 cores), [logs/linux-green.log](logs/linux-green.log)
([linux.sh](linux.sh)): shards-vm 57 of 57, shards-init 212 (1 ignored, as on main), the
workload tests 200 of 200 runs, the guest's end 300 of 300. macOS 26.4 arm64 (the Mac
Studio), [logs/mac.log](logs/mac.log) ([mac.sh](mac.sh)): shards-vm 52 of 52 (the rest are
Linux's), the workload tests 200 of 200 runs.

Mutations, each a guard removed ([mutate.py](mutate.py), [mutations.sh](mutations.sh)), in
the same run, on `f5bfab285`: all red for their reason, [logs/mutations.log](logs/mutations.log)
(the first pass, on `c2c4547c1` with the fix, red alike):

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
- Lint, `f5bfab285`, all seven targets:
  - `scripts/lint` for the four Linux targets, from adacachy (zig 0.17.0): clean,
    [logs/lint-linux.log](logs/lint-linux.log) ([lint-linux.sh](lint-linux.sh)).
  - `scripts/lint aarch64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc` on the
    Mac Studio (zig 0.16.0 by `scripts/install-zig`, cargo-xwin, LLVM, NASM),
    [logs/mac.log](logs/mac.log): macOS clean; it stops at x86_64 Windows on **main's own
    error**, in shards-vmm's test target (`crates/vmm/src/platform/windows_process.rs`
    l. 544, `undocumented_unsafe_blocks`, its SAFETY comment above `assert_ne!` rather than
    the `unsafe` inside it), from cacbd8883 (D181, PM M526), which nothing of this change
    touches.
  - So the rest of `scripts/lint`'s Windows clippy, the workspace less shards-vmm's own
    targets (its library linted still, as the rest's dependency), for both Windows targets:
    clean, [logs/lint-windows.log](logs/lint-windows.log) ([lint-windows.sh](lint-windows.sh)).
- Windows: no test of this change runs there (the workload's tests are Unix's); its build is
  the lint's. AF_UNIX on Windows resets a peer that writes after the other end closed, and
  it loses what it had not read (PM M358): a frame sent just as the guest says its end can
  make that end read as a loss there, said in the log as one; frames are refused and execs
  fail either way, at once.
- Not run: aarch64 Linux and Windows tests (no such host here), any real VM (ROOT's).

## Waits (R05)

- The link's lock was held across a send, as before: a guest that stops reading its signal
  connection blocked every frame's caller until it read again. Reproduced live and fixed in
  the next slice, below: no send waits under the lock.
- The watch waits on the connection's end, a read, no deadline; execs wait on their
  connection, the guest's EXEC_FAILED, the watch's verdict, or the port's close; no new
  timer. `Pending` is no cache: frames waiting for one dial, each standing for one pending
  thing, gone as the dial takes them.
- Not covered: a guest that never dials. Measured and designed in the next slice, below
  (D16's note); not built.

## Next slice: no send under the link's lock (ROOT's items 1 to 3)

From origin/main `e8d19fc68` (the first slice and ROOT's Windows lint fix `13681de5d` in),
same policy SHA-256, implementer; ROOT assigned the live reproduction and one run of each new
E2E test on one host.

### The deadlock, reproduced live (R09)

ROOT's recipe as written cannot happen: the daemon refuses an exec into a paused microVM
("is paused: unpause it to run a command in it", daemon/commands.rs), and a join and a health
check alike. It happens where a pause holds the guest's vCPUs as an exec's frame, more than the
signal connection's host sockets (Linux's default AF_UNIX buffer) and its guest's window
(vsock's 256 KiB) hold, is on its way; then the daemon's relay takes the link's lock for a
signal, and never reads the unpause behind it. `--cpus` cannot slow the guest's reading: the
VM holds its vCPUs in whole CPUs (`vm_run::hold_cpus`), the workload's cgroup alone to the
fraction. The experiments (`stuck-experiments.patch`, run by `e2e-race.sh` on adacachy's KVM at
`e8d19fc68`, the stuck VM's threads taken by its watcher once its output stopped):

- One 5.6 MiB exec and a pause asked 0 to 11 ms after it, then SIGWINCH and an unpause, 60
  times ([logs/race-1.log](logs/race-1.log)): 19 execs refused as paused, 40 run whole,
  and attempt 59 stuck: `shards unpause race did not exit within 30s`.
- 16 exec loops streaming 1.5 MiB frames, pause, SIGWINCH, unpause
  ([logs/stream-1.log](logs/stream-1.log)): stuck in round 0, `unpause` and every exec
  stuck past 120 s.
- The stuck VM process, each time: its `exec` thread in `to_guest` ← `exec_session`,
  holding the link's lock, blocked in `UnixStream::write_all` (kernel: `unix_stream_sendmsg`
  → `sock_alloc_send_pskb`); its `daemon-signals` thread in `Mutex::lock_contended` ←
  `to_guest` ← `warm::relay_signals` (kernel: `futex_wait`); its `pauses` thread waiting on
  its channel for the unpause the relay never passes on; every vCPU held; 14 more `exec`
  threads waiting on the lock.
- At the unit level, with real sockets and threads: `a_send_its_guest_has_yet_to_read_holds_no_other_frames_caller`
  ([lock-red-test.patch](lock-red-test.patch) on `e8d19fc68`, [logs/lock-red.log](logs/lock-red.log)):
  a signal's caller waited on another frame's send, `Err(Timeout)` after 5 s.

Two more facts found on the way, in the first try ([e2e-stuck.sh](e2e-stuck.sh),
[logs/first-try.log](logs/first-try.log)): a signal to a paused microVM whose run names no stop signal
thaws it, as moby's does (docker-v29.8.1 daemon/kill.go l. 80-93), so the tests name one;
and a guest's exec takes no more than a quarter of its 8 MiB stack, 2 MiB, of command line
and environment (`argument list too long` past it), well under a frame's 6 MiB.

### Decision (R02, R04, R05, R12, R13)

One thread, the connection's sender (`send_frames`), owns its sending: frames for it wait in
`Link::Connected(Pending)`, one for each thing pending as before the dial, and it takes each
batch under the lock and sends it with the lock let go. The watch keeps the connection's
reading. A send that fails refuses every later frame at once; where its error says the
guest's end has gone, the watch reads that end and says whether it was said (`SIGNALS_END`)
or lost, and any other error loses the connection there (`lose`), as an end the watch may never
read would leave an exec waiting. Alternatives: one thread owning both directions by poll(2)
(the `platform` seam waits only for reading, `wait_readable`, and dup'd sockets share
`O_NONBLOCK`, so it needs a writable wait on every OS); non-blocking tries under the lock
(the same flag, and a frame left half sent still needs a thread to finish it); a deadline on
a send (none by time). Cost: one thread more per VM process while its guest is connected, its
memory unmeasured (R14, ROOT's).

### Red and green (R09, R10)

| Check | Red at `e8d19fc68` | Green |
| --- | --- | --- |
| unit `a_send_its_guest_has_yet_to_read_holds_no_other_frames_caller` | `Err(Timeout)`, [logs/lock-red.log](logs/lock-red.log) | ok, [logs/unit-green.log](logs/unit-green.log) |
| E2E `a_paused_microvms_unpause_waits_for_no_exec_on_its_way` (8 execs streaming, 5 rounds) | `shards unpause busy did not exit within 60s`, every exec stuck, [logs/pause-red-1.log](logs/pause-red-1.log) | ok in 8.7 s, [logs/pause-green-1.log](logs/pause-green-1.log) |
| shards-vm, shards-init (all) | | 59 of 59; 217, 1 ignored as on main ([logs/unit-green.log](logs/unit-green.log)) |

The guard's mutation, the sender sending with the lock held: the unit test red,
`Err(Timeout)`, and green again once restored ([logs/lock-mutation.log](logs/lock-mutation.log)).
The first slice's link tests now take the sender's thread (`taken`, `send_frames`); a failed
send no longer fails its own frame's caller, which it has answered already (held), but every
frame after it, and any exec waiting, as the watch reads the end.

### Item 2: E2E tests of the owed cases, each run once on KVM (adacachy)

- `a_lost_signal_connection_fails_execs_and_kills_at_once` (containers.rs): an init built
  to take a frame of signal 64 for a malformed one, as init's own malformed-frame path loses
  the connection (`SHARDS_TEST_LOSE_SIGNALS_ON`, `common::lossy_init`, as `foreign_init` is
  built: none in any other build, held by init's `an_init_built_for_runs_takes_no_signal_for_a_malformed_frame`).
  The loss said in daemon.log, an exec fails at once with it, a `kill` ends the microVM at
  once by its VM's stop, and with its daemon killed outright a second run's VM ends at once:
  ok in 51 s, the lossy init's build included ([logs/lost-green-1.log](logs/lost-green-1.log)).
- `what_a_restoring_microvm_is_sent_goes_on_its_guests_dial` (saved.rs, D176): execs sent as
  the restore begins (an exec waits for a run's start, and the VM takes the run before its
  guest dials again, so these are held for that dial when they come first) each run; then
  SIGUSR1 ends the command, 138: ok in 3.1 s ([logs/restore-green-1.log](logs/restore-green-1.log)).
  That a given exec came before the redial is not observable from outside the VM; the held
  path itself is the unit test's (`frames_wait_for_the_dial_one_for_each_thing_pending`).
- `a_runs_end_says_no_lost_signal_connection` (containers.rs): an exec, a signal, a joiner's
  run and the command's end, then a `--rm` run: no loss said: ok in 4.6 s
  ([logs/end-green-1.log](logs/end-green-1.log)).

### Item 3: a guest that never dials

The design is D16's note: init says so on the run connection (`SIGNALS_UNDIALED`), held while
a cut is not open and dropped as a restore is found, as `exec_failures` are; the host loses the
link with it. Today, measured with real threads at the unit level
([never-dialed-experiment.rs](never-dialed-experiment.rs), [logs/never-dialed.log](logs/never-dialed.log)):
an exec asked of a run whose guest never dialed is held (`Awaiting`), still waits after 20 s,
and fails only as the run's end closes the exec port. Not built.

## Rules

| Rule | Evidence / gap |
| --- | --- |
| R01 | No unwrap, expect, panic or indexing in production; `try_reserve` for held frames (`Unheld`); poison-tolerant locks; clippy below. |
| R02 | containerd v2.4.2, moby docker-v29.8.1, Linux 6.18 sources cited above; alternatives (redial, end the run) weighed above. |
| R03 | `NSIG` is Linux's `_NSIG`; the 4-byte ids are the frames' u32s; tests: 5 s bounds an event that comes at once or never. No other literal. |
| R04 | `Link`, `Unsent`, `Lost` typed through callers; transitions above, each exercised. |
| R05 | Waits above; one owner of the link's state; no send under its lock (the sender), the lock wait reproduced live, fixed, mutation-checked. |
| R06, R07, R13 | Small functions (`hold`, `dialed`, `take_signals`, `send_frames`, `watch`, `ended`, `lose`); comments state invariants and sources. The tests' loss hook is one branch, compiled in only where its build names it. |
| R08, R14 | No performance claim; the watch and the sender are two threads per VM process while its guest is connected, their cost unmeasured (ROOT's). |
| R09, R10 | Red, green, mutations above, logs kept. |
| R11, R12 | Above. |
| R15 | Implementer; merged to main by the person's 2026-11-12 rule; real VMs ROOT's. |
| R16 | Work dirs `~/g121-tmp/to-guest` and `~/g121-tmp/to-guest2` on adacachy, `~/to-guest` and `~/to-guest-target` on the Mac Studio: build output, copies and zig, removed after; the stuck experiments' microVMs and daemons, this slice's own, ended by hand. |

## Owed to ROOT

- The four new E2E tests on HVF and WHP (each ran once on KVM), and their whole suites.
- The first slice's real-VM checks not covered by them: a managed run's boundary, joiners'
  execs past a loss.
- Item 3's build (D16's note), with its restore path in a real guest.
- aarch64 Linux and Windows tests.
- The sender thread's memory per VM process (R14).
