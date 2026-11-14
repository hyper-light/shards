# Control findings: focused fixes, 2026-11-13

POLICY_READ: implementer/control, base 9efde72524dd626f3ff84e6b9891852d7c810a9b;
`docs/development-rules.md` SHA-256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Owned findings: control-02 through control-08. Control-01 and the signal-loss investigation
are Claude lanes. Signed commits use GPT-6.1 Sol (Codex) attribution. No local Mac builds/tests.

## Control-02: full Unix socket backlog

The existing Linux `connect` blocked before init dialed the host. Resetting the controller's
ask could therefore not reach that worker. The regression calls the production root-relative
connect with a full real UnixListener backlog, waits for its result, then accepts solely to
release a broken implementation before asserting. No worker survives a red test.

- Red source: bcc79fccdcce043e6f504063463598acfe8664fb.
- Fix source: 3c28b68a78f7deaa9ac3db5046bb4df1ea4c53f1.
- Host: adacachy2, Linux 7.3.3-1-cachyos x86_64; native GNU toolchain.
- Environment: CARGO_INCREMENTAL=0, CARGO_TARGET_DIR=$HOME/g121-tmp/control-target,
  CARGO_PROFILE_DEV_DEBUG=0, CARGO_PROFILE_TEST_DEBUG=0; one cargo at a time, nice -n 10.
- Carrier: git bundle over the pinned base, cloned under $HOME/g121-tmp/fix-control;
  no full-worktree sync, no VM, no E2E, no benchmark.
- Red command (kiln r2): `cargo test -j 4 -p shards-init a_full_socket_backlog_refuses_a_lane_instead_of_holding_its_worker`.
  Actual result: 0 passed, 1 failed; left Err(Timeout), right Ok(Err(11)); the test released
  and joined the blocked worker before failing.
- Green commands (kiln r3): `cargo test -j 4 -p shards-init lanes::tests` (6 passed),
  `cargo clippy -j 4 -p shards-init --all-targets -- -D warnings` (clean).

The fix uses SOCK_NONBLOCK for the root-relative descriptor-path connect, returns its errno
on failure, and restores blocking mode for the established relay. Linux connect(2) documents
EAGAIN for a nonblocking AF_UNIX connection that cannot complete immediately:
https://man7.org/linux/man-pages/man2/connect.2.html (read 2026-11-13). The existing typed
Unreached(errno) carries the refusal to the controller. No retry, timeout or learned value
is introduced, including on a cold run.

Alternatives: a cancellation protocol spanning endpoint, run host, VM and guest would also
have to interrupt the actual guest connect; merely dropping a host reply receiver cannot.
Waiting for backlog space is unnecessary for the socket-lane contract, which already carries
connect refusals. Immediate kernel refusal keeps the existing shared host/guest mechanism.

ROOT owes real-VM coverage of `cargo test -p shards --test nesting a_nested_workloads_sockets_are_lanes_of_its_orchestrators_controller`
and the pending-lane reset/resource-count scenario described in control-02. Only focused
unit reproduction was run here; no claim is made about other pending guest/host operations.

| Rules | Account |
|---|---|
| R01 | Fallible socket creation/connect/mode change; owned fd closes on all errors; pathname checked before filling sockaddr; clippy clean. |
| R02/R05/R12 | Existing Linux socket seam, connect(2) primary contract; alternatives above; no new manager/cache or protocol. |
| R03 | Kernel EAGAIN refusal, no chosen production numeric value or timer; cold start identical. |
| R04 | Existing typed errno/refusal path, no new lifecycle combination. |
| R06/R07/R13 | One root connect function; comments explain ownership and kernel nonblocking behavior. |
| R08/R14 | No performance claim/profile; coordinator measurements owed. |
| R09/R10 | Real Unix backlog drives exact retained-worker symptom, red/green sources and commands above. |
| R11 | Guest is Linux on every host; native Linux x86_64 checked; arm64 and real hypervisor legs owed to ROOT/CI, not passed. |
| R15 | Scoped implementation and light tests only; no agents spawned; signed commits and fresh-main merge before ff landing. |
| R16 | Owned cold target and remote clone only; remove target after landing; no other agent's data touched. |

## Control-03: MCP bridge first failure

Red dee36cb5f8fd70e2e90551ec66b9aa1938dfd60b, kiln r6 on the same Linux host:
`cargo test -j 4 -p shards-init a_bridge_output_failure_does_not_wait_for_its_idle_input`
failed with left Err(Timeout), expected Ok(Err(BrokenPipe)). The test held the actual
stdin pipe open, closed the stdout reader and sent response bytes over a real Unix pair.
Only after capturing the failure did it release stdin and join all workers.

Implementation c02114a2d4374de98d6acd32e3e7cce474eceeca replaces the bridge's two
blocking copies with separate stdio ends in the existing typed native relay. Final source
0a15e5da97dfc038bbb46b40549cbdf1a4c70e3c adds the symmetric EISDIR input error and
preserves the original poll shape for two socket endpoints. Its scopes, partial writes,
backpressure, EOF and faults remain that shared relay's. The bridge owns its stdio ends
and marks them nonblocking; it has no helper to join. Its process lifetime closes them.

Established alternative: Tokio 1.48.0 `copy_bidirectional` returns at the first I/O error
and half-closes each writer on its source's EOF (primary source, read 2026-11-13):
https://github.com/tokio-rs/tokio/blob/tokio-1.48.0/tokio/src/io/util/copy_bidirectional.rs.
No runtime added: the existing Linux relay already supplies that readiness state machine.
Its packet slot bound is derived in relay.rs from Linux VIRTIO_VSOCK_MAX_PKT_BUF_SIZE,
page-rounded on the running guest, with typed allocation/reservation failure and no timer.
The number of readiness entries follows the endpoint descriptors plus the cancel fd.

Kiln r7 passed 5 MCP/5 engine/6 lane tests but clippy correctly failed an unused Read
import; r8 ran that same old snapshot after a transient signing-service refusal. Neither
is claimed a passing gate. The corrected signed snapshot was verified in the carrier
before r9: `cargo test -j 4 -p shards-init mcp::tests` (6 passed), `cargo test -j 4 -p shards-init engine::tests`
(5 passed), `cargo test -j 4 -p shards-init lanes::tests` (6 passed),
`cargo clippy -j 4 -p shards-init --all-targets -- -D warnings` (clean).

R01: existing fallible allocation, checked offsets and typed native errors retained.
R02/R05/R12: extend the existing relay at the concrete socket/stdio seam; sources and
alternative above. No new cache or poll interval. R03: existing packet/page bounds only;
no warm-up or timer, typed failure on cold allocation. R04: Endpoint::Socket/Stdio,
existing Half/SourceEnd/Fault state machines, first fault returned. R06/R07/R13: no
parallel copying implementation, unsafe calls document descriptor/slice obligations.
R08/R14: buffer/CPU costs and performance ratios unmeasured, owed to ROOT. R09/R10:
real pipe/socket red followed by both-direction error regressions and normal EOF green.
R11: the bridge/relay are Linux guest code on every host; x86_64 GNU checked, other
matrix/real-VM legs not run. R15: focused tests only, no agents/VMs. R16: owned target
removed after landing. No guard-removal mutation beyond retained red source was run.

ROOT owes the real-VM MCP restart/fault/cleanup scenario in control-03 and
`cargo test -p shards --test api_image an_orchestrators_members_are_signaled_and_their_mcp_connections_reached_on_its_endpoint`
on its supported hypervisors. No bridge process/fd count in a real VM was measured here.

### Main-merge resolution for control-03

The required fresh-main merge found inithex-03 had independently landed the same first-error
fix in f7d83f73a. Its production implementation and descriptor ownership were preserved;
this lane's shared-relay candidate (c02114a2d/0a15e5da9) was removed from the final tree,
not substituted for another lane's code. Candidate green results above describe the
unlanded candidate only. Upstream proof is retained at ../inithex-fixes/README.md.
The merge keeps an additional real-directory-read-error regression proving that idle
socket output cannot delay an input error. Final merged checks are recorded below.

Final control-03 merged checks: 698a860b9 passed 8 MCP tests, 6 lane tests and clippy
(kiln r11); 9744c78b3, 68e3c8cd1 and 3a3983396 passed 8 MCP tests and clippy after
fresh-main merges (r15/r16/r17). Repeated ff pushes were rejected as main advanced.
A runner push attempt failed for absent GitHub credentials; it changed nothing remote.

## Control-07: member-exec frame bound

Red source 6db1979d61f94d30aa3f9735a696ecf0e31397c1, command
`cargo test -j 4 -p shards-remote an_exec_ask_uses_its_run_spec_bound_beyond_an_ordinary_frame`
on adas-mac-studio.local (Darwin25.2 arm64), old kiln r18: one failed, encoder rejected
4,587,877 bytes at the unrelated 4 MiB cap. Fix 431571292dd9438de364a2ca1cf10f14badcb8df:
MemberExec uses ASK_MOST (the ABI spec bound plus scope) in both put and peek; all other
tags retain MAX_FRAME. Allocation of the framed buffer is fallible, checked before append.
The regression roundtrips the large ask, rejects the same header under an ordinary tag,
and rejects an exec header one past its bound without needing payload allocation.
New-turn kiln r1: `cargo test -j 4 -p shards-remote control::exec::tests` (10 passed),
`cargo test -j 4 -p shards-remote frame::tests` (2 passed),
`cargo clippy -j 4 -p shards-remote --all-targets -- -D warnings` (clean).
Environment and carrier follow the record above; one cargo at a time, no local builds.

R01 checked arithmetic/fallible reservation; R02/R03 ABI Linux exec bound and field widths,
no chosen number or learning/cold-start state; R04 existing typed protocol refusal; R05/R12
existing frame seam with tag-derived admission; R06/R07/R13 one bound helper, no parallel
codec. R08/R14 no performance or allocation-profile claims. R09/R10 actual red encoder and
green reader/writer plus boundary guards. R11 shared source, macOS arm64 run; other targets
owed CI/ROOT. R15/R16 focused tests only, owned target cleaned after landing. ROOT owes a
real member exec with >4 MiB argv under sufficient stack rlimit on its supported VM hosts;
this unit proof establishes framing, not that every guest stack configuration accepts argv.

## Control-04: idle input cancellation

Red a39889e300b07e27efa71b4afec546c42c619ff8, new-turn kiln r7 on
adas-mac-studio.local, Darwin 25.2 arm64:
`cargo test -j 4 -p shards-api an_ended_members_idle_input_releases_its_feeder`
failed with `member end retained its idle input feeder`, Err(Timeout). The test
announces entry into the actual reader, retains its writer, tells INPUT_ENDED,
then releases and joins the reader before asserting the failure. Earlier r4/r5
(missing Read import) and r6 (Rust 2024 opaque lifetime capture) were test-setup
compile failures, not valid red evidence. [Retained red notification excerpt](logs/r7.log).

Fix 9f2ce624afdc62d4f197480c864adae478b67fb3, completed by
151642d8c16229969aa3826efc75aec29a432760: InputFlow owns a clone of the actual
IPC reader. Input-ended, run-ended, answer-channel-ended and session drop call
`shards_ipc::end` on it. The feeder rechecks the authoritative lifecycle after
read, before emitting bytes or EOF. Spawn failure releases the clone and resets
the take flag. The ordinary public PipeWriter interface is unchanged.

The existing IPC Stream seam is used, avoiding a second pump or periodic probe.
Unix converts the pair's writer OwnedFd to PipeWriter; reads use ipc::read for
XNU's half-close behavior (M236). Windows uses its existing overlapped server
Stream and a synchronous client PipeWriter, byte mode: an overlapped client would
violate std::io::PipeWriter's synchronous Write contract. Existing native cancellation
and peer-close detection are retained, not reimplemented. The Windows requested
pipe room is run::CHUNK (the admitted ABI input record), rejects zero, and is a
quota hint, not a new bound on Unix socket memory. Source contracts: Rust std's
`library/std/src/sys/pal/windows/handle.rs` synchronous write and `pipe.rs`, and
Microsoft [CreateNamedPipeW](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-createnamedpipew)
(buffer quota, official contract read 2026-11-13), [synchronous/overlapped I/O](https://learn.microsoft.com/en-us/windows/win32/ipc/synchronous-and-overlapped-input-and-output).
Windows execution and all real-VM propagation are unverified here.

At ea6614b8b, r8 passed all 32 session tests and API/IPC clippy. At final source
76ace2f1842bd9c056d745532be8b5622269d18c, r9 again passed all 32 session tests
(including exact bytes then EOF, final captured output, and idle-input cancellation)
and clippy for shards, shards-api and shards-ipc. The new cleanup test also verifies
a subsequent held-writer write fails. It proves feeder return, not process-wide
fd/heap counts. [Final combined log](logs/r9.log). No guard-removal mutation beyond
the retained red source was run.

## Control-05: bounded cap-loss metadata

Red 738e3212d3ef67b4f4b4237df82ab1aa9d9a81e6, tested in ea6614b8b by r8:
`cargo test -j 4 -p shards a_caps_loss_record_is_bounded_by_its_totals_not_its_history --bin shards`
failed `lifetime eviction history remained on disk`. This drives the actual record
function for 128 losses and checks exact totals and serialized metadata size.
[Retained red notification excerpt](logs/r8.log). It is filesystem evidence, not an allocation profile.

Fix 76ace2f1842bd9c056d745532be8b5622269d18c writes `sum segments bytes` with
checked u64 totals, using existing platform::write_in durable whole replacement
(Unix fsync/rename/directory sync; Windows versioned whole files). Readers use
open_newest_in. A fixed stack chunk and line are each derived from the longest
format line, `sum ` plus two decimal u64s and one separator; with newline the
record is at most 46 bytes. Legacy `seq bytes` histories are streamed with the
same bounded memory and replaced at the next eviction. Invalid data, oversized
lines and counter overflow return InvalidData; no lifetime-sized allocation,
cache, production timer or learned cold-start estimate is introduced.

The existing keeper lock serializes cap updates; concurrent readers get an old
whole or new whole record. Atomic replacement creates mode 0644 on Unix inside
the daemon's existing private home, so its authority boundary remains
that directory; the metadata has aggregate counts only. Windows can retain older
versions while an external reader holds them: bounded current format is proved,
physical retention under Windows holds is not. Legacy open time remains linear
until the next eviction migrates it. No crash-injection claim is made about the
pre-existing segment-removal-before-loss-record order.

R9 at 76ace2f18, same Mac: `cargo test -j 4 -p shards daemon::managed::cap::tests --bin shards`
passed all 5, including exact totals, cross-owner accounting, rotations, legacy
migration, and oversized legacy line refusal. `cargo clippy -j 4 -p shards -p shards-api -p shards-ipc --all-targets -- -D warnings`
clean. [Green](logs/r9.log). For these filesystem-only tests/checks the build used
SHARDS_HELPERS=skip and SHARDS_INIT_BINARY=SHARDS_SERVER_BINARY=/usr/bin/true, preventing
nested guest/helper builds; no helper or VM ran and none is claimed validated.

## Control-06 and control-08: explicitly owed

Control-06 has no actual lifecycle red here. ROOT must extend the existing live
fixture and run:
`cargo test -p shards --test managed_output supplied_owner_output_keeps_bytes_ack_cancel_and_neighbor_progress_independent -- --exact --nocapture`.
The existing test does not establish this defect. Missing witness: attach a run
with drained member output, leave its stderr reader open but unread, establish
that managed-shown entered its blocked write, then stop and remove the run without
releasing that reader; require viewer thread and run-owned fd return, and neighbor
capture progress. Record baseline and changed source plus cleanup after failure.
A fake blocked Write alone cannot prove real-client lifetime behavior. This is
not a claim that daemon process exit waits on that worker.

If confirmed, use a cancellable write adapter tied to the run/viewer lifecycle,
with readiness on both output and cancellation. Do not set O_NONBLOCK on a dup of
client stderr (it shares file-status flags), add a picked retry timeout, or move the
blocked File into another detached worker. Windows needs a native cancellation
state machine that covers cancellation racing entry into a synchronous write;
a CancelSynchronousIo call before entry alone is insufficient. This cross-platform
adapter remains design/implementation work, not a landed fix.

Control-08 is a documented R03 source violation, not a runtime measurement. No
replacement interval was chosen. Design owed: one per-run authoritative producer
notification for append, rotation and terminal/cancel changes; subscribe before
reading current state and recheck it before sleeping. Member logs can notify at
the host keeper. Command logs are appended by the VM process and need a compatible
host notification over an existing control connection. If watch and notification
setup both fail at cold start, return a typed watch-unavailable refusal (requiring
wire compatibility design) rather than invent an initial wait period. Resource
availability can notify retry without a clock. No learning scheme is claimed.

ROOT must add watch-failure injection to the existing managed_output log-lane fixture,
then run `cargo test -p shards --test managed_output log_lanes_read_a_runs_command_and_each_agents_log_at_its_place_and_go_on_from_a_cursor -- --nocapture`, proving append,
rotation, end and lane abort without periodic probes under that fault. That command
alone currently does not inject the fault. Measure idle/cold/load CPU and wakeup
latency only after the notification design is implemented. Until then control-08
remains owed with design, exactly as allowed by fix-brief.md.

## Rules for control-04/05 and outstanding work

| Rules | Proof and remaining gap |
|---|---|
| R01 | Checked counts/line offsets, fallible summary allocation, typed I/O errors; clippy clean. Existing feed AHEAD buffer allocation unchanged; full allocator audit remains ROOT's. |
| R02/R03 | Existing IPC and durable-file seams; native contracts above; ABI CHUNK and decimal-u64 format bounds, no chosen timer. Alternatives: helper copies retain the same cancellation problem; append history cannot bound lifetime storage. No new learned cold state. |
| R04 | InputFlow reader owned until terminal transition; authoritative end rechecked after read. Summary parsing has explicit invalid/overflow errors. |
| R05/R12 | Actual reader is cancelled, no probe/cache/parallel relay. Disk authoritative; cap keeper serializes writers. Native Windows adapter necessary for public synchronous PipeWriter. |
| R06/R07/R13 | Focused factory/close seam, streaming parser, comments explain native cancellation and legacy format; no manager or duplicated codec. |
| R08/R14 | No comparative speed/memory claim, allocation profile or ratios. ROOT owns these, Windows retained versions and process-wide resource counts. |
| R09/R10 | Actual reader Timeout and on-disk growth reds, retained source and logs, green exact lifecycle/bytes/totals guards; no extra mutation run. 06/08 have no runtime red and remain owed. |
| R11 | macOS arm64 tested once per snapshot on the same runner. Linux guest-only 02/03 tested natively x86_64. All unrun Windows, other architectures/libcs and VM propagation remain unverified, CI/ROOT's. |
| R15/R16 | Implementer light tests only, one cargo at a time, named owned clone/targets. No subagents, vendors, benchmarks, sweeps or VMs. Remove owned targets after landing; preserve signed sources/evidence. |

Policy script and seven-target matrix are unrun here: the binding task restricts
implementer commands to focused cargo checks; ROOT/CI own the policy and integration
gates. No outside-area defect was added to this task. ROOT's inbox rejected status
messages with backpressure (pending 4, authority 3); delivery has not been claimed.

## Fresh-main combination and corrected cleanup witness

Signed merge 93aeb4a8ee05418cbf3efcbbca1d0b5c1a70eaf4 brought main f7a3edfbc
without conflicts; policy hashes unchanged. R11 at 7bfa4f376 passed all 5 cap
tests, then 33/34 session tests: the feeder completion message arrived but the
retained writer's immediate write succeeded. The chain stopped; remote tests and
clippy did not run in that invocation. [Failure log](logs/r11-merge-failed.log).
No production cause was inferred from that single observation.

The guard at 4ce3d80a432db5b2ce02d4ff718b8ba84989e43b now joins the completed
feeder before asserting descriptor release; on red it drops the writer first,
then joins before failing. The channel completion message alone was not the
thread-exit witness the test required. At that source, R12 on the same Mac passed
all 34 session tests, 10 exec tests, 2 frame tests, and clippy for shards, API,
IPC and remote. [Complete green log](logs/r12-merged-green.log). Five cap tests
were already green on the identical cap source in R11. This is the focused green
combination over main f7a3edfbc; subsequent doc/comment changes are not executable
changes. R10 never ran a command (carrier folder unavailable); not a test result.

R13 at 4ce3d80a4 on adacachy2 Linux 7.3.3 x86_64: all 8 MCP tests, 6 lane
tests and shards-init all-target clippy passed on a cold owned target, then that
target was removed in the same command. [Linux green](logs/r13-linux-green.log).
Merge 7765f515d81c45c2a39905ce0fc46233945f0cad incorporates main bfd2cca4c;
its only conflict was the platform-measurements append, resolved by preserving
both the control and memory-16 records. No init/ABI/dependency/lock inputs differ
from the native-tested source (git diff --stat 4ce3d80a4..7765f515d over those
paths is empty). Mac focused checks are rerun after this documentation conflict.

R14 on 7765f515d, after the append conflict: 5 cap, 34 session, 10 exec and 2
frame tests passed; all-target clippy for shards, API, IPC and remote clean on
adas-mac-studio.local arm64. [Final merged green](logs/r14-final-merge-green.log).
No executable change follows that test source in this lane's evidence commits.

Additional existing real-VM commands owed to ROOT (not run by this implementer):

- `cargo test -p shards --test api_image a_harness_reads_its_orchestrators_stdin_exactly_and_its_close_is_its_end -- --exact --nocapture` (extend for end/drop with retained writer and native resource return).
- `cargo test -p shards --test api_image stop_ends_the_run_at_once -- --exact --nocapture` (same retained-writer witness).
- `cargo test -p shards --test managed_output the_runs_one_log_cap_is_said_by_logs_and_by_a_lane_from_the_oldest_and_holds_its_bound -- --nocapture` (cap/cursor integration).
- Control-07 still needs a new guest fixture with sufficient stack rlimit and argv encoding above 4 MiB. No existing real-VM test command alone establishes that large-argv case; the unit test proves frame admission only.

The 02 nesting, 03 API MCP and 06/08 missing fault witnesses/commands are specified
in their sections above. All VM commands need ROOT's actual carried helpers, not
the inert embed inputs used in the filesystem-only unit checks here.

The ff push was refused as main advanced. Conflict-free merge
6003be290ebb0359c1fdc8d3749acba7a04153bf incorporates c6dd46525; new main inputs
were net injector, api_run, daemon commands/inspect and vmm's fs server. Following
ROOT's 05:10Z contention amendment, R15 ran
`cargo check -j 4 -p shards -p shards-api -p shards-ipc -p shards-remote --tests`
on the same warm Mac target: clean. [Check log](logs/r15-contention-check.log).
Full focused green remains 7765f515d (R14); native init green 4ce3d80a4 (R13),
whose inputs remain unchanged. No conflict was resolved in this later merge.

A further ff refusal led to conflict-free merge
d95f6104944a37479271c8d64248a66cadc17168 (main 33d28bf88), adding DNS, net and
registry changes plus daemon/build callers. R16 repeated the same four-crate
Mac `cargo check --tests`: clean. Because DNS is also an init dependency, R17
ran `cargo check -j 4 -p shards-init --tests` natively on adacachy2: clean, then
removed its owned target. [Mac check](logs/r16-contention-mac-check.log),
[Linux check](logs/r17-contention-linux-check.log). No test or clippy result is
claimed for this later combination; R14/R13 remain the named full focused greens,
as ROOT's contention amendment requires. This merge resolved no conflicts.

Conflict-free merge 717b0f97417c74eb2d034a7f1aed2a23c7401478 adds main
5f42c9452 (migration moves and vmm guest filesystem changes; init inputs unchanged).
R18: the same warm four-crate Mac `cargo check --tests` clean.
[Check](logs/r18-contention-check.log). Full focused greens remain R14/R13;
DNS-combined native init check remains R17. No later conflict was resolved.

Conflict-free merge 41a972609212d38fa838a844e90fe23ca10bce2f adds main dc23bc71f:
init plug changes and records only. Host crate/dependency paths are unchanged
from R18's 717b0f974; R19 `cargo check -j 4 -p shards-init --tests` is clean on
native Linux, target removed. [Native check](logs/r19-contention-native-check.log).
Full focused green sources remain R14 (host) and R13 (init), not newly run tests.

Merge 30d6080e9b5ad72513cc424bf8edb12d31ed1682 incorporates main b2e3b069a.
Its PM conflict was whitespace beside the control-07 record; the record and
neighboring memory records were retained. As required after any conflict, R22
reran 5 cap, 34 session, 10 exec and 2 frame tests plus all-target clippy for the
same four host crates on Mac Studio arm64: all clean.
[Focused green](logs/r22-merged-focused-green.log). Init inputs unchanged from
R19's checked 41a972609. Source was fetched from the signed checkpoint branch
p/fix-control-v1 into the same owned runner clone, sync:false, pinned to the SHA
above; this avoids the observed long carrier handoff. No credentials moved and
no local build ran. R22 is the latest full host focused green.

Conflict-free merge b4075e66915014fe348badce300203ca92a5bb87 adds main 02d3fe33b
(run end-state persistence only plus records). R23 repeated the same four-crate
warm Mac `cargo check --tests`: clean. [Check](logs/r23-contention-check.log).
Latest full host focused green remains R22 (30d6080e9); init check remains R19,
whose crate/dependency inputs are unchanged. No conflict was resolved here.

Conflict-free merge e51d7eea577b5343c6749030abab5226764e49ed adds main6a52ef516
(migration code). R24 four-crate Mac check clean; immediate ff push from the
runner using its existing SSH identity was refused because main had advanced.
[Check/push log](logs/r24-contention-check-push.log). No authentication failure
or credential transfer occurred. Focused green remains R22, init check R19.

## Landing and cleanup

**Landed main:** f8e5948bc30a3a26390d3fb9b365c370f2a85075. It merges
6df152edc (a net dependency update) without conflicts. R25's four-crate warm
Mac `cargo check --tests` passed, then its immediate SSH ff push advanced main
6df152edc..f8e5948bc. [Successful check and push](logs/r25-landed-check.log).
No extra tests or clippy are claimed at this later merge: full host focused
green is R22 at30d6080e9; native focused green R13 at4ce3d80a4, native init
check R19 at41a972609. Init inputs are unchanged in the later host-only merges.
All later checks followed ROOT's 05:10Z contention amendment; any conflicts
were documentation only and triggered the explicitly recorded focused reruns.

- control-02 fixed/landed (3c28b68a7, landing b8da5eca3); exact full-backlog red/green above.
- control-03 upstream fixed f7d83f73a; this landing adds its symmetric input-error guard, retaining upstream production.
- control-04 fixed/landed (151642d8c completes the cancellation adapter; actual thread-exit witness4ce3d80a4).
- control-05 fixed/landed76ace2f18 (bounded cumulative counters and legacy migration).
- control-06 owed ROOT's live stalled attached-client witness and native adapter design.
- control-07 fixed/landed431571292 (tag-derived spec framing bound).
- control-08 owed with producer-notification/typed refusal design; picked fallback unchanged.

Own first-parent commits from the pinned fix base through this landing:47,
each with exactly one gpgsig header. Final evidence commits use the same signer.
Linux targets were removed by R13/R17/R19 after their commands completed.
R26 on Mac Studio removed only $HOME/g121-tmp/control-target, verified it absent,
exit0; df reports118GiB available. Owned clones and all sources/logs are retained.
No cargo, daemon or VM of this lane remains active. No outside-area review was
started. ROOT's VM/fd/allocation/profile/platform/policy gates remain exactly
as listed above; neither an owned target's removal nor a unit green passes them.
