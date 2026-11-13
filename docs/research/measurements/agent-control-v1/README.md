# The agent-control endpoint (plan step 11)

Kiln plan `01a1cd8e862e7169a074db3d5fa33f3d`, step 11 (items[10]): "kiln (or Hex) drives a
member's stdio, exec and TTY over hyper-transport with TLS13_AES_256_GCM_SHA384 both ways,
authenticated by the VM's own key; a peer without the enrolled identity reaches nothing". The
decision record is [D238](../../../design/architecture.md) ("Agent control over
hyper-transport", its "Building: the endpoint").

## Identity

| | |
|---|---|
| Policy | origin/main `e82b9403b`, read again at `1145bd3f2` for slice 2's landing (same hashes); `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer ([POLICY_READ.json](POLICY_READ.json)) |
| Coordinator | ROOT, session g121, kiln agent `01a148884a4770fdaf0111f3f792a248`; goal `ec1320d898ad86488f95fa5c52d850e1` |
| Handoff | this step's first implementer (`a3b6e82f900fc1cd2`) was stopped by an interruption; its successor kept its uncommitted work and its instruction |
| Source | branch `p/s11-control-v1`, from origin/main `e7825cb32`, fast-forwarded to `e82b9403b`; slice 2 `c9630c6b4`, merged with origin/main `1145bd3f2` as `6fcef62bb` (its Rust byte for byte `c9630c6b4`'s) |
| Owned | `crates/remote/src/control.rs` (and `control/`), its line in `lib.rs` and the handshake tests' helpers there, D238's endpoint text, this directory |
| Not owned | step 6's member exec, attach and TTY (`p/s6-exec-tty-v1`); step 8's paths; step 10's harness (`control-end`, ROOT's) |

## What both arms share, built first

Step 10 is ROOT's and not decided: TLS ending in the guest (arm g) or at one host endpoint
(arm h). ROOT's instruction (2026-11-12): until then, build only what both arms share, with no
speculative abstraction (R05).

| Piece | State | Where |
|---|---|---|
| The connection: ALPN `shards-control/1`, D180's driver and TLS, AES-256-GCM alone | built, slice 1 | `control::{ALPN, endpoint, controller}` |
| The microVM's identity: its own Ed25519 key, pinned both ways (decided over CSR enrollment, R02) | built, slice 1 | D238; `control::{endpoint, controller}` over `identity.rs`, `tls.rs` |
| An unenrolled peer refused, typed | built, slice 1 | `control::{Ended, ended}` |
| A lane's first frame; a lane's refusal | built, slice 1 | `control::{lane, refused, read_refused}` |
| The lanes' service: stdio lanes on step 5's streams, one connection of the controller's at a time | built, slice 2 | `control::{Endpoint, Streams, Served, Revoke}` over step 5's `managed::Service`, `managed::input::Service` |
| The controller's side: kiln's and Hex's client of the stdio lanes (attach included), typed faults, cursors to resume at | built, goal #58's plan step 2 | `control::{Controller, Subscriptions, Output, Input, Fault, Got, Hangup}` (`control/drive.rs`), `net::Handler::cause` |
| The daemon's endpoint per run: `shards.control=true`, a key per run, `control`/`vm` metadata, revoked with the run's scope | built, goal #58's plan step 3 | `daemon::managed::{CONTROL_ANNOTATION, control_asked, bind_agent, Agent}` |
| Exec lanes, the endpoint's side (design (B)): the ask, its streams, its host's start and replies, its steering, its exit | built, goal #58's plan step 4 slice (a) | `control::exec`, `net::Io::limit` |
| The controller's execs: `Execs::exec`, the exec's stdio as `Output`/`Input`, `ExecControl` (resize, signal, wait) | built, goal #58's plan step 4 slice (b) | `control::{Execs, Exec, ExecControl}` |
| The daemon's exec host: an exec charged to the run's budget, run in its member through its VM process, its start typed (`EXEC_TOLD`), its stdio pumped, its steering passed on, hung up as its controller goes | built, goal #58's plan step 4 slice (c) | `daemon::managed::execs`, `Daemon::{launch_exec, hand_exec}`, `shards_ipc::{EXEC_TOLD, kind::EXEC_UNSTARTED}`, `workload::exec_session`, `control::exec::lane_bytes` |
| Signal, inspect, terminal and log lanes | goal #58's plan steps 5 to 7 | — |
| Log lane; where the endpoint runs and how its key's SHA-256 reaches the controller | held for step 10 | — |

## Decision (R02, R03, R05, R12)

D238 has the sources and the comparison: kiln's CSR enrollment (`crates/kiln/src/pki.rs` at
d6ffa45d), Hex's (`docs/design/hex-quic.md` Q6 at d84730ee), Kubernetes' kubelet TLS
bootstrapping, D180's pinned raw keys (RFC 7250), WireGuard's static keys (NDSS 2017 §1, §2).
Decided: the microVM's own key, pinned, because nothing but its launcher vouches for a
microVM's key either way (RFC 7250 §6's out-of-band binding), a pin puts no grant inside the
microVM and needs no path from it to a CA, and a pin's revocation is its removal. Falsified by
a party that must verify a microVM without its launcher; then a CSR certifies the same key.

The wire is hyper-quic under D180's driver, not hyper-raft's hyper-transport crate: a member's
stdio is a byte stream of no known length, which a QUIC stream carries natively (its FIN,
RESET_STREAM and flow control), where the crate's lanes are one-way streams that last as long
as their connection and its exchanges need their bodies' lengths (Hex built a connection
protocol over them for that, hex-quic.md Q3).

Sources read for it, as fetched: RFC 7250 (sha256 `834f7695…38dd`), RFC 8446 (`47871bc8…b3bd`),
RFC 9001 (`3bbaecdf…a591`) from rfc-editor.org's text; the WireGuard paper from
wireguard.com/papers; the kubelet TLS bootstrapping page from kubernetes.io; kiln, Hex and
hyper-raft at the revisions named.

R03 (the person, 2026-11-13: "we do not tolerate arbitrary constants. learned and data-derived
values. Period."): every literal is a protocol's or step 5's (D238's table). Slice 1 sets no
window, buffer, chunk, keep-alive or idle value: `endpoint` and `controller` take their
transport from their caller.

## States and refusals (R01, R04, R06, R07, R13)

- `control::Ended`: how a control connection ended, read from QUIC's codes, with which side
  said so; callers match it. `NotEnrolled` is TLS's access_denied, raised by the pinning
  verifiers on both sides before any 1-RTT packet is read (RFC 9001 §5.7).
- `control::Lane`: a lane's first frame, judged before anything after it is read; a frame of
  another kind, length or channel is `Reason::Protocol`. A lane's refusal is a `Refused` frame
  of a `managed::Reason`'s code.
- Panic review: no unwrap, expect or slicing in production code; every split checked
  (`split_first_chunk`, `get`), sizes from `size_of`.

## Light tests

`cargo test -p shards-remote --lib`, on each host in [Results](#results); `control::tests`:

- `an_enrolled_controller_and_its_pinned_vm_connect_on_aes_256_gcm`
- `a_controller_not_enrolled_is_refused_at_its_handshake`
- `an_endpoint_not_the_pinned_vm_is_refused_at_its_handshake`
- `a_peer_of_another_protocol_is_refused`
- `a_close_reads_as_its_cause`
- `step_5s_subscriptions_open_a_members_lanes`
- `a_lane_of_no_kind_is_refused`
- `a_lanes_refusal_reads_back_as_its_reason`

Guard-removal mutations: `python3 -I mutate.py SRC_ROOT [NAME_PREFIX]`, run from a checkout of the
branch with `CARGO_TARGET_DIR` set. Slice 1's six guards: the endpoint's pin of its controller, the
controller's pin of the microVM, the protocol, access_denied's reading, a lane frame's length, a
lane's channel. Slice 2's nine, each named with the test that should fail without it: the newer
connection's `Stale` close of the older, one lane per output and per stdin, a refusal of the lane
alone rather than its connection, a revoked output's lane reset alone (`Stop::Lane`), a frame
refused after its input's end stopping only the lane's reading, the connection window's sum, a
record beside a queued wake keeping its place, and a finished output's lane stopped both ways. The
last is marked UNCERTAIN: its test opens a lane after the output finishes, but stdin's earlier
close may return the stream credit that lane needs, so the mutant may live, which would mean no
test holds that guard. Slice 2's mutations were written and each guard text checked to occur once,
but never built or run: the person's rule of 2026-11-13 makes mutation runs ROOT's.

## Results

Each with its host, revision and command; logs in [logs/](logs/), aws-lc-sys's build chatter left
out. Slice 1 is `2a3b62356` (on origin/main `e82b9403b`), built cold in a target directory of
its own on each host, from a clone of that commit (`git bundle` of the branch, its sha256
checked).

| Host | Ran | Result |
| --- | --- | --- |
| Linux x86_64 (adacachy, CachyOS 7.2.9, rustc 1.98.1) | `cargo test -p shards-remote --lib`; `cargo clippy -p shards-remote --all-targets -- -D warnings` | 52 passed (8 of them `control::tests`); clean ([log](logs/slice1-linux-x86_64-test-clippy.log)) |
| the same | `cargo fmt --all --check`; scripts/lint's Linux arms for the crate changed, `cargo clippy -p shards-remote --all-targets --target T -- -D warnings` with zig 0.17.0 as C compiler, T each of x86_64 and aarch64, gnu and musl | clean ([log](logs/slice1-linux-lint.log)) |
| the same | `python3 -I mutate.py SRC` | 6 of 6 mutants killed, each by its test's assertion failing (no build failed); each restored green ([log](logs/slice1-linux-mutations.log)) |
| macOS 26.2 arm64 (adas-mac-studio-2, Darwin 25.2.0) | `cargo test -p shards-remote --lib`; `cargo clippy -p shards-remote --all-targets -- -D warnings` (aarch64-apple-darwin) | 51 passed (the 8 `control::tests` among them; `net::tests::an_interrupted_receive_keeps_serving_commands` is Linux's alone); clean ([log](logs/slice1-macos-arm64.log)) |
| Windows 11 x86_64 (adacachy's guest, 10.0.26200, MSVC 14.44) | `cargo test -p shards-remote --lib`; `cargo clippy -p shards-remote --all-targets -- -D warnings` | 51 passed (the 8 `control::tests` among them); clean ([log](logs/slice1-windows-x86_64.log)) |
| macOS arm64 (adas-mac-studio-2), cargo-xwin with Homebrew's LLVM | scripts/lint's Windows arm for the crate changed: `cargo xwin clippy -p shards-remote --all-targets --target aarch64-pc-windows-msvc -- -D warnings` | clean ([log](logs/slice1-windows-aarch64-lint.log)); MSVC's own ARM64 compiler on the Windows guest cannot build aws-lc's C, as scripts/lint's use of clang-cl implies |

Slice 2 (the stdio lanes' service; this record's next commit), Linux x86_64 (adacachy, rustc
1.98.1), its source over origin/main `455f8cd1a`: `cargo test -p shards-remote --lib` 62 passed
(17 `control::tests`, 8 of them the lanes' over UDP on loopback; step 5's `managed` tests and
the new `a_record_submitted_beside_a_queued_wake_is_kept_for_its_turn` among them);
`cargo test -p shards-remote --test loopback` 17 passed; `cargo clippy -p shards-remote
--all-targets -- -D warnings` clean; the daemon's and api-run's tests of step 5's endpoints,
`cargo test -p shards --bin shards -- managed api_run` (`SHARDS_HELPERS=skip`, placeholder guest
binaries), 35 passed. One loopback run of the 17 failed once on the loaded host
(`what_a_command_of_a_connection_gone_writes_reaches_no_other`, its 60 s deadline), then passed
three runs of the change and three of the base each: D180's, not this change's.
Slice 1's merged tree (`455f8cd1a`) also passed on macOS arm64 (adas-mac-studio-3): 51 passed,
clippy clean.

Slice 2's merged tree (`6fcef62bb`, whose Rust is `c9630c6b4`'s), each run on a cold clone with a
target directory of its own, removed after:

| Host | Ran | Result |
| --- | --- | --- |
| Linux x86_64 (adacachy2, CachyOS 7.3.3, rustc 1.98.1, zig 0.16.0) | `cargo test -p shards-remote --lib`, `--test loopback`; `cargo fmt --all --check`; clippy for x86_64 gnu and musl and aarch64 musl | 62 passed (17 `control::tests`), 17 passed, fmt clean, clippy clean ([log](logs/slice2-linux-x86_64.log)) |
| Linux x86_64 (adacachy, CachyOS 7.2.9, rustc 1.98.1, zig 0.17.0) | clippy for aarch64-unknown-linux-gnu (not installed on adacachy2) | clean ([log](logs/slice2-linux-aarch64-gnu-lint.log)) |
| macOS 26.5.2 arm64 (adalundhe-macbook-pro, M5 Max) | `cargo test -p shards-remote --lib`, `--test loopback` | 61 passed (`net::tests::an_interrupted_receive_keeps_serving_commands` is Linux's alone), 17 passed ([log](logs/slice2-macos-arm64.log)) |
| macOS 26.2 arm64 (adas-mac-studio-2, M4 Max) | clippy for aarch64-apple-darwin; `cargo xwin clippy` for aarch64- and x86_64-pc-windows-msvc | clean, clean, clean ([log](logs/slice2-macos-lint.log)) |
| Windows 11 x86_64 (adacachy's guest, 10.0.26200, MSVC 14.44, rustc 1.98.1) | `cargo test -p shards-remote --lib`, `--test loopback`; native clippy | 61 passed, 17 passed, clean ([log](logs/slice2-windows-x86_64.log)) |

Every clippy above is `SHARDS_HELPERS=skip cargo clippy -p shards-remote -p shards --all-targets
--target T -- -D warnings` (shards is the only crate that depends on shards-remote), with
placeholder guest binaries (`SHARDS_INIT_BINARY`, `SHARDS_SERVER_BINARY`), which slice 2 does not
touch, and scripts/lint's C toolchain for each target: all seven targets are clean.

The first run of `mutate.py` reported its kills by exit status alone; the harness now counts a
mutant killed only where its one test ran and failed, and prints the failure, and the log is
that second run's.

The controller's side (goal #58's plan `01a1d2c733ea7731bb3204cad78bc94d`, step 2), on one host
as the person's rule of 2026-11-13 asks, its tree origin/main `b66bc0131` with `4c50a8744` merged:
macOS 26.2 arm64 (adas-mac-studio.local, M4 Max, rustc 1.98.1), `cargo test -p shards-remote
--lib` 68 passed (24 `control::tests`, seven of them the controller's), `--test loopback` 17
passed, `cargo clippy -p shards-remote --all-targets -- -D warnings` clean, rustfmt clean. CI on
main covers the other targets.

The daemon's endpoint per run (plan step 3), the same host and toolchain, on `a8eee1382` with the
change applied: `cargo test -p shards-remote --lib control::` 24 passed; `cargo test -p shards
--bin shards -- daemon::managed` (`SHARDS_HELPERS=skip`, placeholder guest binaries) 27 passed,
6 ignored (ROOT's native fixtures, as before), the two new among them; `cargo clippy -p
shards-remote -p shards --all-targets -- -D warnings` clean; rustfmt clean.

Exec lanes' endpoint side, the same host and toolchain, on `17ed8a2e5` with the change applied:
`cargo test -p shards-remote --lib control::` 32 passed (8 new: the ask, steering and told
frames, exec scopes; an exec over its lanes with a stand-in host; asks refused and an exec not
started; an endpoint with no exec; a connection gone hanging its exec up); `cargo test -p
shards --bin shards -- daemon::managed::tests` 12 passed; `cargo clippy -p shards-remote -p
shards --all-targets -- -D warnings` clean.

The controller's execs, the same host, on `c5ba4f454` with the change applied: `cargo test -p
shards-remote --lib control::` 33 passed (the new: the controller execs into a member, its stdio,
steering and exit, and one not started); `cargo test -p shards --bin shards --
daemon::managed::tests` 12 passed; `cargo clippy -p shards-remote -p shards --all-targets -- -D
warnings` clean.

The daemon's exec host, the same host (adas-mac-studio.local, macOS 26.2 arm64, rustc 1.98.1),
on `fe353a203` with the change applied, `SHARDS_HELPERS=skip` and placeholder guest binaries:
`cargo test -p shards-remote --lib control::` 33 passed; `cargo test -p shards --bin shards --
daemon::managed` 31 passed, 6 ignored (ROOT's native fixtures), the four new among them (the exec
host through the run's endpoint and a stand-in VM process; a member's exec takes its command and
stdio alone; a guest's refusal typed by its class; a pipe's buffer measured or bounded); `cargo
test -p shards-vm-process --bin shards-vm -- workload::tests::a_told_exec` 1 passed; `cargo clippy
-p shards-ipc -p shards-remote -p shards -p shards-vm-process --all-targets -- -D warnings` clean;
rustfmt clean. Again on the tree that lands, origin/main `f8d5a4913` with the change merged (its
kind `EXEC_UNSTARTED` 96, as D247 took 94 and 95), the same host: `control::` 33 passed; `cargo
test -p shards --bin shards -- daemon::managed daemon::health` 38 passed, 6 ignored (health.rs's
`send_exec`, split for the host, among them); `a_told_exec` 1 passed; the same clippy clean.

Each end's word on its TLS (the control lane's report, D238; ROOT, 2026-11-13), on origin/main
`24d64a414` with the change merged, and again on `ae583f055`, the tree that lands (the same counts), adas-mac-studio-3.local (macOS 26.2 arm64, rustc 1.98.1),
`SHARDS_HELPERS=skip`: `cargo test -p shards-remote --lib control::` 35 passed, the two new among
them (the control lane answered with the endpoint's own TLS13_AES_256_GCM_SHA384 and its group,
an ask of more than its tag refused alone and the next answered; the controller's `Link` the same
at both ends, its member's lanes going on) and `the_controllers_windows_are_its_lanes_own_summed`
counting the lane's report and ask; `tls::` 6 passed (a negotiation the provider's for that suite
alone, AES-128-GCM, ChaCha20-Poly1305 and codes rustls names none of not); `frame::` 2 passed;
`cargo test -p shards --bin shards -- daemon::managed` 37 passed, 6 ignored (ROOT's native
fixtures); `cargo clippy -p shards-remote -p shards --all-targets -- -D warnings` clean; rustfmt
clean on the files changed.

D254's `Mcp` asks (the endpoint's and the controller's) and D180's values derived, together, on origin/main
`039f5019c` with the change applied, adas-mac-studio-3.local (macOS 26.2 arm64, rustc 1.98.1),
`SHARDS_HELPERS=skip`: `cargo test -p shards-remote --lib` `control::` 36 passed (an MCP
connection through a stand-in host, its bytes exact both ways, its end told, a member with no
bridge refused), `tls::` 7 (a handshake waited for until R1 at its first probe timeout, 15 s for
a peer never measured), `frame::` 2, `net::` 1 (a handshake held to its bound alone),
`managed::` 30; `cargo test -p shards-remote --test loopback` 17 passed (a stranger let go at its
connection's bound, no daemon answering told at the handshake's, an enrolled client's command
running past a stranger's bound, 8 MiB crossing whole); `cargo test -p shards --bin shards --
daemon::managed context::` 50 passed, 6 ignored (the daemon's host refusing an MCP ask before
its VM process hears of it); `cargo test -p shards --test context_client` 6 passed, 1 ignored
(a command whose output nobody reads hung up there, through the client's relays without a
high-water mark); `cargo clippy -p shards-remote -p shards --all-targets -- -D warnings` clean;
rustfmt clean. Red before: the same `control::` run on the ask code as it was hung in the new MCP
test, its binary waiting 13 minutes at 0.45 s of CPU (an ask the endpoint's first read took
whole, never read on: D254, "Built"), until its processes were ended.

D180's link timing under the person's ruling of 2026-11-13 (D180, "Its values derived": what a
home knows of each daemon's path, `tls::Known`, kept per context; a cold start with no deadline,
its probes told; liveness at use, no sooner than the probe timeout; keep-alives only where a
path lost a connection to silence or a peer negotiated an idle timeout, hyper-quic patch Q34;
strangers' places and handshake slots with no deadline), over origin/main `e0cc059c7`,
adas-mac-studio-3.local (macOS 26.2 arm64, rustc 1.98.1), `SHARDS_HELPERS=skip`: `cargo test -p
shards-remote --lib` `control::` 44, `tls::` 7, `frame::` 2, `net::` 6, `managed::` 30 passed;
`cargo test -p shards-remote --test loopback` 19 passed; `cargo test -p shards --bin shards --
daemon::managed context:: client_binds::` 53 passed, 6 ignored; `cargo test -p shards --test
context_client` 6 passed, 1 ignored; `cargo clippy -p shards-remote -p shards --all-targets --
-D warnings` clean; rustfmt clean. Red before, each run with prints of each death (not kept):
10 of the loopback tests' 19 taken for dead on a send 28.9 ms unanswered (a window of 26.9 ms
from a 1.9 ms handshake and 25 ms of the daemon's max_ack_delay; its probe timeout 30.7 ms),
then, with waits judged no sooner than the probe timeout, 8 still, the daemon's driver silent
from its handshake's end: its first start built the tests' helper with cargo, on the driver;
then one, a resize never served, the client's wake datagrams dropped by its socket aimed at
the daemon (two sockets waited on since); and `context_client`'s save cut short by a file size
limit saying the record's failed write first (no word of it since).

D180's link under load (ROOT's runs: `tests/context.rs` at 084891178, 8 of 21 on HVF and 1 on
KVM, and at c1ef455c9, 6 on HVF, a daemon reached before given up within its slowest handshake;
`tests/loopback.rs`'s closed port, 22 minutes on KVM at 237f3ebe7; `moved.rs`'s image left
unnamed on KVM): a reached-before handshake judged by its window no sooner than its probe
timeout; a driver's absence, the time its thread neither ran nor waited as planned, held against
no peer; its socket read non-blocking; an aimed socket's refusal read from a send too; a moved
image named there or its move failed. Over origin/main `084891178`, `SHARDS_HELPERS=skip`:
adacachy2 (Linux 7.3.3-1-cachyos x86_64, rustc 1.98.1): `cargo clippy -p shards-remote
--all-targets -- -D warnings` clean; `cargo test -p shards-remote --lib` `net::` 10, `tls::` 8,
`control::` 45, `managed::` 27 passed; `--test loopback` 25 passed. adalundhe-macbook-pro (macOS
26.5 arm64, rustc 1.98.1): `cargo clippy -p shards-remote -p shards --all-targets -- -D
warnings` clean; the same lib suites (`net::` 9, the Linux signal test not built), `--test
loopback` 25; `cargo test -p shards --bin shards -- daemon::managed context:: client_binds::
cli::moves::` 61 passed, 6 ignored; `--test context_client` 6 passed, 1 ignored. Red first, each
cure undone alone, on both hosts: the slowest handshake a deadline from its start (the window's
unit test; the relay stand-in's slower second handshake given up at 1.2-1.7 ms); no absence
taken (its unit test; the self-frozen process's command died in 5 of 5 rounds on each host,
"closed by peer: unanswered past the slowest answer seen"); a moved image's naming answers let
go (its unit test); and on Linux a send's refusal let go (the closed port given up by its caller
at 63 s, 16 probes). Measured on adacachy2 under kiln: 400 sleeps of 2 ms took p50 2.105 ms,
p90 2.113, p99 123.97, max 124.02 (Python's `time.sleep`, `perf_counter`; timer slack 100 µs,
sched_ext off). On the way, two stand-in designs failed for the hosts' freezes and not the
code: a relay holding every datagram of the daemon's slept 58 ms past its time (a 2.2 ms hold),
its daemon taking a live client for dead; and a freeze test whose client sat in a jiffy's
receive timeout each turn heard its daemon in it, so its red was luck (both replaced: the relay
holds a handshake's answer alone, released at once; the socket is read non-blocking).

## R01–R16

| Rule | Evidence / gap |
| --- | --- |
| R01 | Panic review above; clippy with the workspace's denied lints on each target linted. |
| R02 | D238's comparison, with its sources and their revisions. |
| R03 | No new literal: D238's table; no transport value (the caller's). |
| R04 | `Ended`, `Lane` and the lane's refusal typed; callers match variants. |
| R05 | No new trait or layer: `endpoint` and `controller` are D180's contexts under this protocol's name. No wait or cache added. |
| R06, R07, R13 | One module of codecs and two constructors; comments carry sources and invariants. |
| R08, R14 | No performance claim; handshake CPU, per-VM memory and allocations are ROOT's (step 10's harness measures this connection). |
| R09, R10 | A new capability: no defect claimed reproduced. Red/green: each guard's mutation, above. |
| R11, R12 | Pure Rust over D180's driver, one implementation on every target: tests on Linux x86_64, macOS arm64 and Windows x86_64, clippy on all seven targets (Results); tests on aarch64 Linux and aarch64 Windows owed. |
| R15 | Implementer: light tests only; no real VM run. |
| R16 | Build output under `~/g121-tmp/s11-control` on each runner, removed after. |

## Owed

- Slice 1's light tests on aarch64 Linux and aarch64 Windows (linted only): no such host ran them (no native arm64 Linux or Windows runner; Docker Desktop's arm64 Linux is owed with slice 2's UDP tests).
- Slice 2's guard-removal mutations, ROOT's: `python3 -I mutate.py SRC_ROOT` from a checkout of
  main once slice 2 is on it (all 15, slice 1's six again with them). The finished output's stop
  is UNCERTAIN (above).
- Slice 2's light tests on aarch64 Linux and aarch64 Windows (linted only), as slice 1's.
- The plan of the rest is kiln plan `01a1d2c733ea7731bb3204cad78bc94d`: the controller's side of the stdio lanes (attach is step 5's subscriptions, so these lanes are attach's), the endpoint where step 10 puts it, exec lanes on step 6's member exec, member signal and inspect, a member's own terminal, the log lane.
- ROOT's: every real-VM run, R14's costs (handshake CPU, per-VM memory, allocations), the
  adversarial campaign (floods, slow readers, hostile frames).
- The control lane's report: each end's refusal of a connection whose suite its provider does not
  offer is held by no test, as no peer of this crate's provider negotiates another suite; its
  guard mutations (the endpoint's and the controller's `is_provided` checks) are ROOT's, as the
  report's own are (the endpoint answering a refusal instead, the controller reading no report).
  kiln's session record of both ends (goal 60's relay) is read in step 34's E2E, ROOT's.
- D254's owed: the member's listener and bridge (D251's agent); the run hosts' pairing of each
  connection with its ask (this goal's, after the listener lands); kiln's server over the two
  streams (kiln's step 18); every real-VM run of a harness calling kiln's server through the
  bridge, ROOT's. Guard mutations, ROOT's: the endpoint's `Tag::Mcp` arm, `Kind::Mcp`'s stdin and
  missing stderr, and the whole ask's read of no room (`read_exec`).
- D180's values: quinn-proto's stream and connection windows, its send window and the 100
  streams a daemon's client may open are left as hyper-quic sets them (D180, "Its values
  derived"). The same liveness for step 5's and D238's endpoints, which still admit `QUIET`
  (this goal's next slice, ROOT's call C), goal 75's `unix::link()` with it; a relay's engine
  the handshake slots' memory term. A relay's engine client gone mid-handshake is seen at its
  next probe since (relay.rs `gone`), and a host's start is off the daemon's driver
  (`a_slow_start_holds_up_no_other_client`, red with it on the driver). hyper-quic's patch Q34
  (`Connection::idle_timeout`, VENDORED.md §50) is on hyper-raft's main (045cf3ff, Shards
  pinned to it): numbered Q30 on its first branch, a number main's §38 had taken. Guard
  mutations, ROOT's: the
  probe-timeout floor, the handshake's counts at its end, the first flight told as no probe,
  the wake socket, the record's address check, the reader's credit at an empty queue, the
  one-slot queue (none deadlocks).
- D180's link (ROOT's): `cargo test -p shards --profile test-release --test context`, `--test
  moved` and `--test context_client` on HVF and KVM. A driver's absence on Windows is its poll's
  lateness past its timer alone, its thread times moving at the clock interrupt: a freeze in a
  turn's midst there is not taken (the freeze test is Unix's, SIGSTOP).
- The exec host's real-VM run, ROOT's: no E2E drives a run's agent-control endpoint yet. Its place
  is `crates/shards/tests/managed_output.rs` (Linux; a submodule beside `member_stdin.rs`): a run
  started with `--annotation shards.control=true`, its controller dialed at the metadata's
  `control` and `vm` lines with the home's key, an exec in the harness member (stdin echoed, an
  exit status, a terminal resized and `stty size` read back, a signal), and one refused by a
  member that has ended. Its macOS, Windows and aarch64 runs with it; the slice's guard
  mutations (`member_spec`'s refusal, `EXEC_TOLD`'s `STARTED`, the pump's buffer before its
  record), R15 excluding them from an implementer.

## Goal 58's steps 5 and 6: members' own lanes

Kiln plan `01a1d2c733ea7731bb3204cad78bc94d`, items 5 and 6: "the controller signals a member and
reads its state and end; refusals typed", and "a harness's own terminal UI driven over the
lanes with resize". D238's "Members' own lanes", "The controller's members" and "The daemon's
member host" record what was built; D244's daemon's run is amended.

### Identity

| | |
|---|---|
| Policy | origin/main `e74cee6b7`, read again after the 15:06Z restart at `dc7f45bc6` (same hashes): `AGENTS.md` `f66d304c…`, `docs/development-rules.md` `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, `docs/development-evidence.md` `61613a24…`; role implementer, a child of goal 58's agent (`a5cbff00b30d2e4f0`), POLICY_READ sent to ROOT |
| Source | branch `p/s11-members-v1`, based on origin/main `845d7eb93`; WIP commits squashed at landing (below) |
| Owned | `crates/remote/src/{frame.rs,control.rs,control/exec.rs,control/drive.rs}`'s member lanes; `crates/shards/src/daemon/managed/members.rs` and its hooks in `managed.rs`, `managed/execs.rs`, `daemon.rs`; abi's `builtin::SIGNAL` and `members::signal_args`; init's `SIGNAL` built-in (`run.rs`, `managed.rs`'s two accessors), agreed with goal 75's agent (no run or IPC kind, nothing in its signal-connection dispatch) |
| Numbers | frame tags `Member` 8, `Running` 43, `Steered` 44; `Unsteered` 19 to 22; `builtin::SIGNAL` 22, `SIGNAL_ENDED` 3, `SIGNAL_UNKNOWN` 2 (reservations.md); no D, no image channel version, no run or IPC kind |

### Decision (R02, R03, R05, R12)

- *Signal by init's built-in*, not a run kind of its own (goal 58's agent approved,
  2026-11-13): D244's `RESIZE` built-in sizes a member's terminal already, so both steers share
  one host path (`members::run_builtin`) and one init path (its built-in dispatch); no run or IPC
  kind, nothing in the VM process. The kept alternative, a `MEMBER_SIGNAL` on the signal
  connection answered on the workload connection, is cheaper per signal and is the one to
  measure against (owed, below).
- *Its target*: the member's command, PID 2 under its reaper, as an OCI runtime's kill signals
  its container's process (runtime-spec runtime.md, "kill"). Not the reaper: from init's
  namespace a signal reaches a namespace's init only for a handler it has (pid_namespaces(7)),
  and the reaper (init managed.rs `reaper`) is sealed with SIGCHLD's alone, its seccomp filter
  allowing no signal call of its own.
- *Race-free*: each candidate is opened as its `/proc/PID` directory, its status read through it
  and its signal sent with pidfd_send_signal(2) on it, which takes such a directory
  (pidfd_send_signal(2), "a file descriptor that refers to a process ... obtained by opening a
  /proc/pid directory"); a path descriptor (`O_PATH`) would not do, as `fdget` skips one. The
  candidates are the member's cgroup's (`cgroup.procs`, cgroup v2), on every guest kernel members
  need, not `/proc/PID/task/TID/children` (CONFIG_PROC_CHILDREN, which shards' kernels have but a
  guest kernel a person configures (D28) need not).
- *Answers per steer*: OCI's kill of a container that does not run must fail (runtime.md), which
  Hex's member `signal` maps onto (`Awaits(MemberSignal)`), and R04 asks init's typed refusal to
  reach its caller; an exec's steers stay told taken, unchanged.
- *Ends as they come*: `member-ends` asked of a run with agent control, its init's
  `MEMBER_ENDED` (already relayed by its VM process) kept in the run's end table, a condvar its
  lanes wait on (R05: an event, no probe).
- *No constant chosen* (R03): every new number is the protocol's own next (tags, codes, the
  built-in) or a cited bound (ESRCH, ENOENT, NSIG, PID 2); every charge is measured as an exec's is.

### States and refusals (R01, R04, R06, R07, R13)

The endpoint's lane: `Asking`, `Pending`, `Running` (each steer handed, then awaited, then its
answer staged), `Exited` (told once no steer awaits its answer), `Told`. Its frames are staged
only as the one before has gone whole. Refusals typed at each layer: the lane's (`Reason`), the
host's (`Unstarted::Capacity`/`Failed`), each steer's (`Unsteered::Ended`/`Terminal`/`Capacity`/
`Failed`), init's exit by kind (`SIGNAL_ENDED`, `SIGNAL_UNKNOWN`). No `unwrap`, `expect`, index
or unchecked arithmetic in production code; clippy with the workspace's denied lints.

### Found and fixed (R09, R10)

`exec_turn` staged an exec's `Exit` over its `Started` where both came in one turn (a silent exec
whose outputs finished at once), which its controller took for a broken protocol. Reproduced
before the fix with the test alone on main (`an_exec_that_starts_and_exits_at_once_is_told_both_in_order`
on `f2f4517ba`, which is `845d7eb93` and that test): `left: Exit(0)`, `right: Started(0)`, 1 of 1
failed (adas-mac-studio-3.local, macOS 26.2 arm64, rustc 1.98.1; [log](logs/members-red-macos-arm64.log)).
Green with the fix (below). A second liveness defect, of this slice's own and found before it
landed: a member's exit due once its last answer had gone was not staged until another event woke
the endpoint (the controller's test waited on it unbounded); fixed by staging each next frame as
the one before goes whole, held by `a_members_lane_tells_it_runs_answers_each_steer_and_tells_its_end`.

### Light tests and results

Each on the merged tree `e8f578800` (`8bb81584d` and `991d741e9` merged with origin/main
`740fa3726`), from a clone of a `git bundle` of the branch over `845d7eb93`, in a target directory
of this lane's own:

| Host | Commands | Result |
|---|---|---|
| adas-mac-studio-3.local, macOS 26.2 arm64, rustc 1.98.1 | `cargo test -p shards-remote --lib control`; `cargo clippy -p shards-remote --all-targets -- -D warnings`; `cargo clippy -p shards --all-targets -- -D warnings`; `cargo test -p shards --bin shards -- daemon::managed`; `cargo test -p shards --bin shards -- every_entry_the_composers_spell` | 54 passed; clean; clean; 41 passed, 6 ignored (ROOT's native fixtures); 1 passed ([log](logs/members-macos-arm64.log)) |
| adacachy2, Linux x86_64, rustc 1.98.1, `x86_64-unknown-linux-musl`, nice 10 | `cargo test -p shards-init --target … member_signal_tests`; `cargo test -p shards-abi --target … members`; `cargo clippy -p shards-init -p shards-abi --target … --all-targets -- -D warnings` | 4 passed (the namespace test ran: util-linux 2.42.4, unprivileged user namespaces on); 18 passed; clean ([log](logs/members-linux-x86_64-musl.log)) |

Red before green: [members-red-macos-arm64.log](logs/members-red-macos-arm64.log), above. rustfmt
clean on every file changed.

### R01–R16 (this slice)

| Rule | Evidence / gap |
| --- | --- |
| R01 | No panic path in production code; clippy with the workspace's denied lints on macOS arm64 (shards-remote, shards) and Linux musl (shards-init, shards-abi). |
| R02 | D238's "The signal's alternatives"; OCI runtime-spec runtime.md ("kill"), pid_namespaces(7), pidfd_send_signal(2), proc(5) `NSpid`, cgroup-v2 `cgroup.procs`, Hex's shards-image-class.md §2 (`Awaits(MemberSignal)`). |
| R03 | No chosen constant: protocol numbers next in their own sequences; ESRCH, ENOENT, NSIG, PID 2 cited; charges measured (D238's literals). |
| R04 | Lane stages and every refusal typed (`Unsteered`, `Unstarted`, `Reason`, init's exits by kind); callers match variants. |
| R05 | One new module (the daemon's member host) at the existing exec seam; the end table is an event (a condvar its lanes wait on), no probe, no cache. |
| R06, R07, R13 | A member's lane is the exec lane's machinery with one optional `Steering`; comments carry sources and invariants. |
| R08, R14 | No performance claim; the built-in exec per steer is unmeasured, owed to ROOT. |
| R09, R10 | The exec lane's `Started` overwritten: reproduced red, green after (above). The member's late exit: a defect of this slice's own, before it landed, held by its test. Guard mutations: ROOT's (below). |
| R11, R12 | One implementation for every target (Rust over D180's driver; init's Linux code for both guest architectures); tested on macOS arm64 and Linux x86_64 musl; the rest of the seven targets owed to CI on main. |
| R15 | Implementer: light tests only, no real VM. |
| R16 | Build output under `~/g121-tmp/s11-members` on each runner, removed after landing. |

### Owed

- ROOT's real-VM runs (the run's endpoint over a real microVM), in `crates/shards/tests/managed_output.rs`
  beside the exec host's owed run: a run with `--annotation shards.control=true` and
  `--annotation shards.output=harness:NAME`, its controller dialed at the metadata's `control` and
  `vm` lines; `Execs::member` of the harness (`Member::Running`), `MemberSteering::signal(15)`
  taken and the harness's end read from `MemberEnd::wait` (143 for a command that dies of it, or its
  own status); a member whose command has ended refused `Ended`; on Linux KVM, macOS HVF and WHP.
- ROOT's real-VM terminal run (step 6's acceptance): `--annotation shards.terminal=harness:NAME`
  with `shards.stdin` and `shards.output` naming it; a resize over its lane, then `stty size` typed
  into its terminal and read back from its stdout; a terminal UI's redraw at the new size.
- R14: the built-in exec per steer, against a signal on the signal connection (the alternative
  kept): n, p50/p90/p99/max of a signal's answer and a resize's, from the controller's side.
- Guard mutations, ROOT's: the endpoint's one steer in flight (`Steering::ready`), the staging
  of each next frame as the one before goes (`exec_turn`'s loop), the answer before the exit
  (`stage`'s `awaiting`), init's PPid and NSpid checks (`command_state`), its zombie, its
  `/proc` directory as the pidfd (the test's PID 3 sibling holds the target), and the daemon's
  `member-ends` ask.
- api-run (D252): its endpoint admits no exec, so no member lane yet; Hex's `MemberSignal` over
  an orchestrator's run needs api-run to host member lanes with this host (goal 75's, or this
  goal's next).
- Hex and kiln: shards-remote's `control::Fault` gains `Unsteered`; a re-vendored caller that
  matches `Fault` exhaustively must add the arm.

## Goal 58's step 7: log lanes

Goal 58's plan, item 7 (design L1, ROOT's): a run's logs read over the agent-control link, every
member's output kept in its log owned or not, within the run's one log cap. D238's "Log lanes"
records what was built; D240 what is kept. Built in slices, each landed as it went green.

### Identity

| | |
|---|---|
| Policy | origin/main `744dd57cc`: `docs/development-rules.md` `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer, a child of goal 58's agent (`a5cbff00b30d2e4f0`), POLICY_READ sent to it |
| Source | branch `p/s11-logs-v1`, based on origin/main `744dd57cc` |
| Owned | `crates/remote/src/{frame.rs,control.rs,control/exec.rs,control/drive.rs}`'s log lane; `crates/shards/src/daemon/managed/` (the log host and the keeper of members' logs); `daemon/logs.rs`, `log_writer.rs`, `spec.rs`'s log constants, `segments.rs` (the run's one cap), `daemon/files.rs` (its one call), `daemon/packed.rs` (members' segments), `daemon/commands.rs` (`logs`'s notice); D238's and D240's text |
| Numbers | frame tags `Log` 9, `Rotated` 45; `Unlogged` 23 to 25 (reservations.md); no D, no image channel version, no run or IPC kind |

### Decision (R02, R03, R05, R12)

- *Asked as an exec is* (L1): a log lane is `exec::Kind::Log`, so its records ride the exec
  machinery already built and reviewed: one stream of step 5's records (offset and bytes, one in
  flight under the controller's credit), its own lane opened at `Started`, its end told once its
  records finished, its windows the run's exec admission. No second record protocol (R12).
- *Byte-exact, not by lines*: the log's records as written, each at its own place (`LogPos`,
  the store's `Pos`), so that a lane goes on from any byte. `logs`'s and the shipper's reader
  holds a line until it ends and goes on from a line's start; a harness's prompt, written with
  no newline, would wait there, and a lane resumed inside a line would lose its rest.
- *Typed in-band events*: what was not kept (a gap, typed by why) and what was let go before the
  lane read it (`Rotated`) are records, not words, so a controller matches them (R04).
- *No constant chosen* (R03): tags and codes are the protocol's next; every size is a field's
  own (the place's 24 bytes, a byte per flag); records and windows are the run's exec admission.

### States and refusals (R01, R04, R06, R07, R13)

The endpoint's lane is an exec lane's (`Asking`, `Pending`, `Running`, `Exited`, `Told`), with a
`log` flag: only a log lane is told why it reads none (`Unlogged`); a host's `Unlogged` for any
other lane is told as `Unstarted::Failed`. The controller's: an exec lane's with `log`, its
records held to `LogRecord::check` (a record that does not read breaks its protocol), `Hangup` as
its `Log` is let go before its end. No `unwrap`, `expect`, index or unchecked arithmetic in
production code; clippy with the workspace's denied lints.

### Light tests and results

| Slice | Host | Commands | Result |
|---|---|---|---|
| 1, the lane and its controller | adalundhe-macbook-pro, macOS 26.5 arm64, rustc 1.98.1 | `cargo test -p shards-remote --lib control::`; `cargo clippy -p shards-remote -p shards --all-targets -- -D warnings` | 56 passed (5 new codec tests, the loopback log test); clean ([log](logs/log-lane-slice1-macos-arm64.log)) |

Red before green, slice 1: the endpoint refusing the log of a member it serves no stream of
(`scoped()` without its log exception): `left: Err(Refused(Scope))`, `right:
Err(Unlogged(Unknown))` ([log](logs/log-lane-slice1-red-macos-arm64.log)).

### R01–R16 (so far)

| Rule | Evidence / gap |
| --- | --- |
| R01 | No panic path in production code (checked arithmetic, `get`, chunked splits, no indexing); clippy clean with the denied lints. |
| R02 | D163's cursor (the store's `Pos`) for places; step 5's records reused; the line reader's resumption read in `daemon/logs.rs` (why a lane cannot use it). |
| R03 | No chosen constant: the protocol's next numbers, fields' own sizes, the run's exec admission. |
| R04 | Every refusal and event typed (`Unlogged`, `LogRecord`, `Gapped`); callers match variants. |
| R05 | No new seam: the exec lane's; no cache. |
| R06, R07, R13 | One `log` flag on each side's exec lane; comments carry sources and invariants. |
| R08, R14 | No performance claim; owed to ROOT. |
| R09, R10 | New behaviour, not a defect: its test's red (above). |
| R11, R12 | One implementation for every target; tested on macOS arm64; the rest owed to CI on main. |
| R15 | Implementer: light tests only, no real VM. |
| R16 | Build output under `~/g121-tmp/s11-logs` on the runner, removed after landing. |

### Owed

- ROOT's real-VM run, once the daemon's log host lands (next slice): a run with
  `--annotation shards.control=true`, its controller's `Execs::log` of the run's command and of a
  member, unfollowed and followed, resumed from a cursor; on Linux KVM, macOS HVF and WHP.
- Hex and kiln: shards-remote's `control::Fault` gains `Unlogged`, and `Granted` gains `Log`; a
  re-vendored caller that matches either exhaustively must add the arm.
