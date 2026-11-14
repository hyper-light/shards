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
