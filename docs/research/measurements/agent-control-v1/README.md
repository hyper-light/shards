# The agent-control endpoint (plan step 11)

Kiln plan `01a1cd8e862e7169a074db3d5fa33f3d`, step 11 (items[10]): "kiln (or Hex) drives a
member's stdio, exec and TTY over hyper-transport with TLS13_AES_256_GCM_SHA384 both ways,
authenticated by the VM's own key; a peer without the enrolled identity reaches nothing". The
decision record is [D238](../../../design/architecture.md) ("Agent control over
hyper-transport", its "Building: the endpoint").

## Identity

| | |
|---|---|
| Policy | origin/main `e82b9403b`; `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer ([POLICY_READ.json](POLICY_READ.json)) |
| Coordinator | ROOT, session g121, kiln agent `01a148884a4770fdaf0111f3f792a248`; goal `ec1320d898ad86488f95fa5c52d850e1` |
| Handoff | this step's first implementer (`a3b6e82f900fc1cd2`) was stopped by an interruption; its successor kept its uncommitted work and its instruction |
| Source | branch `p/s11-control-v1`, from origin/main `e7825cb32`, fast-forwarded to `e82b9403b` |
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
| The lanes' service: stdio lanes on step 5's streams | next slice | — |
| Control (exec, signal, inspect) and terminal (attach, TTY) lanes | held for step 6 | — |
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

Guard-removal mutations: `python3 -I mutate.py SRC_ROOT` (six guards: the endpoint's pin of its
controller, the controller's pin of the microVM, the protocol, access_denied's reading, a lane
frame's length, a lane's channel).

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

The first run of `mutate.py` reported its kills by exit status alone; the harness now counts a
mutant killed only where its one test ran and failed, and prints the failure, and the log is
that second run's.

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
- The lanes' service on the driver, stdio lanes served from step 5's slots, and its loopback
  tests over UDP (next slice).
- Exec, signal, inspect, attach and TTY lanes on step 6; the log lane and the arm on step 10.
- ROOT's: every real-VM run, R14's costs (handshake CPU, per-VM memory, allocations), the
  adversarial campaign (floods, slow readers, hostile frames).
