# A member's stdin, stdout and stderr as streams its owner holds (plan step 5)

Kiln plan `01a1cd8e862e7169a074db3d5fa33f3d`, step 5: "a HARNESS member's stdin, stdout and
stderr are streams its owner holds, with exact bytes, backpressure, EOF and abort, and
authorized observers". It extends the managed-runtime plan's P3 (exact-byte capture, the
authenticated owner endpoint, resume) with G24's input ownership. The member's stdout and
stderr were already its owner's (P3, [managed-output](../managed-output/README.md)); this
adds its stdin, by both of its owners: the daemon's endpoint (`shards run`), and `shards
api-run`'s orchestrator (shards-api, Hex's path).

## Identity

| | |
|---|---|
| Policy | origin/main `451672250`; `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer ([POLICY_READ.json](POLICY_READ.json)) |
| Coordinator | ROOT, session g121-46, kiln agent `01a148884a4770fdaf0111f3f792a248` |
| Source | branch `p/s5-stdio-v1` from origin/main `451672250`, merged with origin/main `d35c4cf79` (the exec-port admission fix it builds on) |
| Owned | the member's stdio descriptors in crates/init and their relay (init, VM process), the daemon's observers, `shards api-run`'s input, shards-api's `take_input` |
| Not owned | step 7's harness_env, ENV, HOME, working directory and mount order; step 8's paths and shares; step 9's grants; step 6's exec, attach and TTY; `hellos.rs` admission (exec-admission) |

## Decision (R02, R03, R05, R12)

The design is D231 decision v1's §3.3 ([record](../../runtime-image-boot-decision-v1.md)),
read before any edit: input closed unless its owner asks as the run is made; the member's
stdin the read end of a pipe init holds; the input on a connection of its own that init
dials to the exec port with a token the setup names, "so vsock's own credit backs it up and
an unread stdin holds up nothing else"; the VM process handing that connection to its owner
as it hands the engine's. Built as decided; [architecture.md](../../../design/architecture.md)
("A member's stdin, a stream its owner writes") has the states and the literal table.

Alternatives weighed:

- **Records on the managed capture connection**, a credit per member each way (output's
  shape mirrored). One connection would carry every member's input and output, so an unread
  stdin is held off the others only as long as every party drains that connection; it adds
  a credit protocol to init, the VM process and both owners. Rejected for the decided
  connection of its own, whose flow control is vsock's (virtio 1.3 §5.10.6.3; the device
  reads a host socket only while its guest has credit, `conn.rs` `interest`).
- **containerd's and Docker's stdin** (containerd pkg/cio: a FIFO the shim copies into the
  process's stdin; moby attach: a hijacked connection, CloseWrite its end) and **Kubernetes'
  remotecommand** (stdin a stream of its own, v5 a close signal for half-closing it): raw
  bytes on a stream of their own, an end by close. Shards keeps that shape between init and
  its owner, and adds what they lack for an agent's owner: cursors, so that an owner that
  reconnects goes on without losing or repeating a byte, and a typed end with the count the
  member's stdin took.
- **The opt-in.** shards-api's `MemberSpec::stdin`, designed in D231 v1 (refused
  `Unsupported` until now); for the daemon, the run's annotation `shards.stdin` (OCI
  runtime-spec config.md "Annotations"; Kata's `io.katacontainers.*`, gVisor's
  `dev.gvisor.*`), shards' own key as `shards.egress` is a network's (D192), so that no Docker
  flag's meaning changes and no oracle table moves. A harness is not given a stdin by
  default: Codex's `exec` reads its stdin to its end (harness-bringup-v1 H5) and would wait
  for an owner that never writes.

R03, each bound an existing one or a kernel's: a record at most `run::CHUNK` (one `STDIN`
frame, what the guest reads of a stream at once, as a command's and an exec's stdin frames
carry); init's relay buffer the member pipe's own capacity (`F_GETPIPE_SZ`); the token the
exec port's (`run::TOKEN`); frame sizes their fields'; patience the output endpoint's
(`QUIET`, its keep-alive a quarter). R12: one implementation on every host; the hand-over is
the engine connection's (D181), on Windows its pipe relay (`relay_engine`) reused.

## States and code (R01, R04, R06, R07, R13)

- **init** (`crates/init/src/input.rs`): `Dialing → Hello → Relaying → Ended(InputEnd) →
  Told`, every failure into `Ended`, which closes the pipe and the connection at once. The
  member's end is seen idle as the pipe's POLLERR (fs/pipe.c `pipe_poll`) and writing as
  EPIPE. Reports go on the managed control connection (`MANAGED_INPUT_ENDED`), their room
  reserved with the scope's (`ControlQueue::prepare(members, inputs)`). Typed ends:
  `Closed`; `Aborted(EPIPE)`; the connection's errno (its connect, hello, read), `ECONNRESET`
  for an end without the owner's; `EPROTO`; `ENOMEM`.
- **VM process**: a token per input member, minted, registered (`Target::Input`), forgotten
  (`hellos::forget`) where its setup never goes; connections handed over by `MANAGED_INPUT`,
  the end relayed checked once a member.
- **Owner endpoint** (`crates/remote/src/managed/input.rs`): `Free → Coming → Whole →
  Relaying → Free`, `Free → Ending → Closing → Ended`, an abort into `Ending` from any open
  state, `Revoked` as the run's scope ends. Subscription `Listening → Connecting →
  Subscribed`, any of them back to `Listening` on a loss, which lets go of a record cut short
  and nothing else.
- **daemon** (`daemon/managed.rs`): an `Input` per member (its endpoint's control, the
  member's connection `Awaited → Here → Gone`), a relay thread writing each record as one
  `STDIN` frame, the endpoint's driver; the end appended to the run's metadata record.
- **api-run** (`api_run.rs`): a `Feed` per member (its connection `Awaited → Here → Gone`,
  what the orchestrator sent, its room, its end), a writer thread per member; the wire's
  `INPUT`, `INPUT_END` to the host, `INPUT_ROOM`, `INPUT_ENDED` from it (shards-api wire
  version 4). shards-api's `take_input` feeds a pipe within its room.

Panic review: no unwrap, expect or slicing in production; counts checked or saturating;
buffers reserved fallibly (`try_reserve_exact`) before use; locks poison-tolerant.

## Light tests

[Results below](#results), each with its command and host.

- abi `run::managed`: input entries (active, ascending, unique, one token each, none without
  an entry, cut tokens refused), the end's codec.
- init `input` (6): exact binary, no-newline, invalid UTF-8 across split frames; an unread
  stdin stops the connection's reading and resumes as read; EPIPE idle and writing; a
  connection cut short, a frame of another kind, a bad header; a hello that cannot go; the
  hello's token and the relay's size. `capture`: the end's reserved room, told before the
  scope's end.
- remote `managed::input` (6): frames; a record taken whole once the last is delivered;
  refusals by offset, room, scope, run, generation and channel; resubscription a record
  behind; an abort held by an unread record closes its connection; revocation; and over
  real QUIC on loopback a stranger's key refused, exact bytes, a record held until
  delivered, a vanished owner's record going on and its next subscription told where to go
  on, the end and the stream's FIN.
- shards-api wire and session: the input's frames both ways, the appended flag, room and end
  taken for asked members alone, a feed sending its caller's bytes within its room and its
  end (`cat` as its host).
- Written for ROOT's real-VM suite: `crates/shards/tests/managed_output/member_stdin.rs`
  (a harness echoing its stdin as JSON lines and bytes past a pipe's capacity, a stranger
  refused, a resumed owner, its end; a member that ends, its input `EPIPE`).

## Results

Light tests, `cargo +1.98.1 test` with `SHARDS_HELPERS=skip` (no guest built), at the branch's
heads named; every count the filter's whole, every new test among them. `998375151` is the
branch with main at `9e5e5f8ea` merged in, whose change landed as one commit on main past it
(main's later commits touching none of its files' code); `58b6dade4` the same but for the
stdin E2E test's member naming its working directory (main's `MemberSpec::workdir`).

| Host | Revision | Ran | Result |
| --- | --- | --- | --- |
| Linux x86_64 (adacachy, CachyOS 7.2.9) | `58b6dade4`, `998375151` | init `input` 7 and `capture::` 7 (13 in all); vm-process `workload::capture warm::` 6; shards `managed api_run` 35; api `image` 18; `tests/managed_output` and `tests/api_image` built | all pass |
| Linux x86_64 (the same) | `6537321dc`, `fe62523be` | remote `managed` 29 (6 input, one over real QUIC); abi `managed` 8; ipc kinds 1 | all pass |
| macOS 26 arm64 (Mac Studio) | `998375151` | abi 8; ipc 1; remote 29; api 18 (the feed with `cat` as its host); vm-process 6; shards `managed api_run` 35 (both daemon input tests); the two E2E targets built | all pass |
| Windows 11 x86_64 (adacachy's guest, 10.0.26200) | `998375151` | abi 8; ipc 1; remote 29 (the input endpoint over real QUIC); api 17 (the feed's test is Unix's); vm-process `warm::` 2, its Windows hand-over built; shards `managed api_run` 34 (the daemon's and api-run's Windows input arms built) | all pass |

The first Windows run (`6537321dc`) found the endpoint's tests naming Linux's errnos through
`libc`, which crates/remote has on Unix alone: the guest's errnos are Linux's on every host,
so the tests now name them as constants of their own (`d120abe2f`). A run at `58b6dade4`
built the VM process against a stale shards-abi (main's `NSIG` and `SIGNALS_END` not found)
and counted api's tests of before: the guest's clock runs 8 h ahead of its host's, so sources
copied with their host's mtimes read older than its last build and cargo kept it. The run
above extracted its copy with mtimes of the guest's own (`tar -m`), and rebuilt every crate
of the workspace.

Guard-removal mutations ([mutate.py](mutate.py), each its guard's test failing with the guard
gone and passing restored):

| Guard | Test | Host | |
| --- | --- | --- | --- |
| abi: an input member is active | `input_members_are_active_ascending_and_each_has_one_token` | macOS | killed |
| remote: a record starts where the last was taken | `a_record_is_taken_whole_once_the_last_is_delivered…` | macOS | killed |
| remote: a record is at most its room | the same | macOS | killed |
| remote: a resubscription is never ahead of what was taken | `a_lost_subscription_lets_go_of_a_record_cut_short…` | macOS | killed |
| daemon: an input member runs a program | `input_members_are_named_by_kind_and_name…` | macOS | killed |
| VM process: an input's end is relayed once, for an open input | `an_inputs_end_is_relayed_once_for_an_open_input_alone` | macOS | killed |
| init: the connection is read only once all it gave is in the pipe | `an_unread_stdin_stops_reading_the_connection…` | Linux | killed |
| init: a member no longer reading ends its input while idle | `a_member_that_stops_reading_ends_its_input_as_epipe…` | Linux | killed |
| init: an end without its owner's is no owner's end | `a_connection_that_ends_without_its_owners_end…` | Linux | killed |

`scripts/lint`, every supported target, at `998375151` on the Mac Studio (zig 0.16.0): format,
then clippy on aarch64-apple-darwin, x86_64 and aarch64 Linux (gnu and musl) and Windows,
clean.

## R01–R16

| Rule | Evidence / gap |
| --- | --- |
| R01 | Above; clippy with the workspace's denied lints on each target linted. |
| R02 | D231 v1 §3.3 and the alternatives above, with their sources. |
| R03 | No new literal: each bound an existing one or the kernel's, table in architecture.md. |
| R04 | The states above; typed ends on every wire (`InputEnd`, `StreamEnd`, `Said`, `Next`). |
| R05 | Waits: the relay on its slot's condvar (records, ends), the endpoint's driver on readiness, init on poll; no cache. The output path unchanged. |
| R06, R07, R13 | Each state machine its own module or struct; comments for sources and invariants. |
| R08, R14 | No performance claim. Throughput, latency, allocations and the per-member threads' cost are unmeasured: ROOT's. |
| R09, R10 | A new capability: no defect claimed reproduced. The red-by-design live check: the endpoint refusing an over-room record was seen first as a test failure and kept as a guard. |
| R11, R12 | Per host below; one implementation, Windows by the engine's pipe relay. |
| R15 | Implementer: light tests only; real-VM E2E written for ROOT, not run. |
| R16 | Build output under `~/g121-tmp/s5-*` (adacachy), `~/s5-*` (Mac Studio). |

## First real-VM runs, and their fixes (2026-11-13)

The first real-VM runs found every run asking a member's stdin refused before its start:
- HVF (ROOT, main 27ed7cccd, macOS 26 arm64): `api_image`'s stdin case broke its pipe, "the
  harness said Some(\"\") and ended Ok(Some(WithRun)); the run's end Ok(Some(Stopped))";
  `member_exec`'s terminal-with-stdin case saw its terminal close before `ready`, the run
  `Stopped`. Log `/tmp/g121-shot/e2e-hvf3-27ed7cccd.log`.
- Linux CI (x86_64 gnu, 5b4a561e5, run 43385596626 job 131753477108): `managed_output`'s two
  `member_stdin` runs exited 125, "shards: " with no words after it.

The cause was the VM process's `workload::capture::start`: it checked that a request brings
no input token with `managed::input_tokens`, which holds a setup to one token for each
member `managed-input=` names, so a request asking one input (no token yet) was refused,
"managed output member was not admitted", said on the VM's stderr alone. 72a204fb9 checks
for a `managed-input-token=` entry instead (red with its test alone, c0d1dd752; green after).
On the same path, shards-api made a member's input flow only once `Session::start` ran
again after the reader took `STARTED`, which a room (`INPUT_ROOM`) the reader took first
broke; 35f12476b makes the flows with the start, under its lock (red/green
`a_stdins_room_right_after_the_start_is_its_members`). Both landed in 22101e85e.

At main 41d35bde1 (with 22101e85e), ROOT's HVF run: the `api_image` stdin case and
`member_exec`'s terminal-with-stdin case pass. `managed_output`'s `member_stdin` runs on
Linux alone: owed, CI's next x86_64 run.

## Owed to ROOT

- The real-VM E2E above on KVM x86_64 and arm64, HVF and WHP: `tests/managed_output.rs`'s
  `member_stdin` and the shards-api path's, `tests/api_image.rs`'s
  `a_harness_reads_its_orchestrators_stdin_exactly_and_its_close_is_its_end`; written and
  built, not run.
- Light tests on aarch64 Linux and aarch64 Windows (linted only): no such host here ran them.
- Cost (R14): the two threads per input member in the daemon and in api-run, the endpoint,
  and init's buffer.
- A typed input end on shards-api's `ImageGuest` (the caller sees its pipe's EPIPE).
- Found, not this step's: a member that `domains.rs` starts keeps init's SIGPIPE ignored (Rust
  ignores it in init, and execve keeps an ignored signal ignored), where an exec's child sets
  every signal to its default (`run.rs`): a member writing to a reader that has gone sees
  `EPIPE` where a process started by a shell would die of SIGPIPE.
