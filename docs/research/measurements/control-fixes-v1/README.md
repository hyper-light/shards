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
