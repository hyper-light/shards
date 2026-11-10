# Supplied-member owner-output fixture

This is a runnable fixture checkpoint, **not a real-VM passing result**. The
coordinator owns the real KVM/HVF/WHP, mutation, stress and resource campaigns under
the latest user bootstrap. The implementer owns only the new focused integration
test, its pinned dev-dependency/lock edge and this measurement record. No production
behavior, endpoint allowance, diagnostic API or retirement policy changes here.

Base: combined `5d231e1b36ad37f9de7451bce1207a4fc859153b`. Policy: canonical
`c921a0b12b06d495b354907162e991376edffd41`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Role: implementer. Source:
[`managed_output.rs`](../../../../crates/shards/tests/managed_output.rs).

## Contract and actual seam

The fixture builds an Agentfile with a files-only AGENT followed by a runnable
AGENT directory and a runnable HARNESS archive. Both programs are the existing
static `testguest managed-capture` workload. They write their member label plus
different binary tails to fd1/fd2, including NUL, invalid UTF-8, a terminal escape
and a final fragment without a newline. Tests compare those bytes in memory;
failure diagnostics show lengths/offsets rather than raw payloads.

`run vm -d` returns only after trusted setup published the private owner metadata.
The fixture reads that file once after the synchronous reply, loads only its own
temporary home's identity, verifies the server/owner fingerprints and checks the
files-only hole: member IDs 1 and 2 each have independent stdout/stderr endpoints;
member 0 has none. Scope uses the daemon's actual run ID and generation. It uses
the existing public `managed::client_config` and `net::Driver` APIs over real UDP/
TLS. The test-only hyper-quic dependency is exactly the already-pinned source
`f116ac276d2d53296de24c2a43837baf66ed1cbd`, needed to name public callback types.

The positive case leaves AGENT stdout unacknowledged while that member's stderr
delivers its exact bytes and returns application credit. It then closes the held
observer locally and executes a new main-workload command before and after
cancellation. Both HARNESS channels receive their first subscription/credit only
after local observer completion, and must then deliver and acknowledge their own
exact bytes. This tests sibling capture progress after the local cancellation;
it does not infer a server retirement event from client completion.
Each main command's stdout/stderr must remain exact. Owner
revocation must complete and durable terminal metadata must distinguish the held
channel's accepted-but-unconsumed bytes from the other channels' consumed bytes.
This proves only the effects actually observed when the coordinator runs it; the
fixture's existence is not proof of capture/lifetime independence.

An ACK is followed by FIN on the same ordered QUIC stream. The fixture waits for
the existing typed `Reason::Closed` response: `Service::readable` processes the
consumption frame before its end branch. This witnesses server processing without
sleeping or reading cached capacity. QUIC's ordered-byte and final-size contracts
are [RFC 9000 §2.2](https://www.rfc-editor.org/rfc/rfc9000.html#section-2.2) and
[§3.1](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.1); this does not add a
new application barrier or imply that a transport ACK is application consumption.

A separate VM case uses a fresh first connection to each channel: wrong owner,
stale generation and cross-member scope must produce no application bytes;
another member/channel and main exec must still progress. Scope/stale closes match
typed managed reasons. The current public Driver erases the TLS error variant, so
the wrong-owner case observes a peer close without an application code and rejects
local failures/deadlines/data. It does **not** classify the exact TLS alert from
message text. That API limit and the independent auth-guard mutation remain owed.

These cases do not reconnect after a rejection or cancellation. They therefore do
not hide or purport to fix the independently reproduced immediate reconnect gap
in the [native observer record](README.md). There is no limit increase, retry
probe, retirement shortcut, new manager or cache.

## Ownership and waits

Event channels are constructed before each client thread. Each phase uses one
absolute test deadline; unrelated events cannot extend it. Driver readiness and
required QUIC deadlines remain authoritative. No new periodic state read, top
loop, retry sleep or duplicated credit store is introduced. Existing CLI test
helpers still contain their historical child-process supervision polling; this
checkpoint does not alter or claim to remove that separate path.

Failure is caught before owner/client cleanup, then reported again. Owner stop
revokes the real daemon run; each local observer also receives its existing stop
notification, completion is awaited and threads are joined. Local client FD counts
are compared only after observer senders/workers drop. These counts do not measure
daemon/kernel charges, task counts, parent memory, or general leak freedom. The
same-process registry helper retains its own process-lifetime listener.

The patience of 120 seconds is an explicit fixture bound matching adjacent build
tests, not production policy. Record/transport windows derive from published
capacity; datagram size uses the existing protocol bound. Client handshake space
matches the daemon's RFC 8446 uint24 handshake-length bound plus header. Neither
test workload values nor old retirement measurements enlarge admitted authority.

## Commands and owed coordinator proof

Light implementer checks are limited to formatting/policy, selected cargo check/
clippy and the pure metadata parser test. No VM campaign has run here.

| Source / kiln run | Actual result |
| --- | --- |
| `e3e2a517` / r23 | Selected cargo check exit0, 72 seconds. |
| `e3e2a517` / r25 | Strict selected clippy exit101: four test-helper `expect_used` errors and one array-chunk preference. Retained, not a passing gate. |
| `b22ea508` / r26 | Corrected strict clippy exit0; pure parser1 passed/0 failed. First test-profile build247 seconds; parser itself0.00s. |
| `842460aa` / r29 | Strict clippy exit0; pure parser1 passed/0 failed,3 unrelated tests filtered out. Rebuilt selected test in1.81s. Neither VM case executed. |
| `3752fc9f` / r31 | Final fixture cleanup handling: strict clippy exit0; pure parser1 passed/0 failed. Selected test rebuilt in1.80s. VM/helper-failure cleanup remains unverified. |

Final fixture source SHA-256:
`4551ae4c605e35e8f18616373712803eb9f205636ce0deb73ff2ab88179fae2c`.
Fedora host `fedora.attlocal.net`, Linux6.19.10-300.fc44.x86_64, glibc2.43,
Rust1.98.1 (`48a229cea`), LLVM22.1.8. All requests retained4CPU/8GiB/10GiB.
Logs, actual exit files, environment and hashes for all four source checkpoints
are retained under [`owner-output-checks/`](owner-output-checks/); its SHA256SUMS
covers20 raw files plus the command record. Local named-file rustfmt, diff and
policy checks passed.
The original failed lint result is preserved alongside the corrected source.

Exact light invocations are in
[`owner-output-checks/commands.txt`](owner-output-checks/commands.txt). The private
compiler target is `~/managed-owner-output-target`, evidence outside it at
`~/managed-owner-output-evidence`; the original endpoint reflink source was not
modified. No job remains active at this checkpoint. Mac actual free space at the
final check was45,840,860KiB, above the30GiB floor. The wrapper signature header
and required attribution are present; no certificate trust-chain claim is made.

Composition note: root's newer combined `cbe3d027` aligns the remote transport to
tested hyper-quic `233b7fcef7a363327c5439b191724a38065062cd`. This fixture remains
at the assigned 5d base for its light checks. On composition, root must align the
new **dev-dependency** revision with that same remote/lock source; two different
hyper-quic sources would give incompatible callback `StreamId`/`ClientConfig`
types. No second version or new workspace abstraction is intended.

The coordinator must use a private clone/target with actual carried helpers and
guest binaries; `SHARDS_HELPERS=skip` is valid only for the light compile check.
Keep the assigned VM envelope 4 CPU / 8 GiB memory / 20 GiB disk, existing headroom
guard, no host root and supported host availability rules. For example:

```sh
env -u SHARDS_HELPERS CARGO_TARGET_DIR="$HOME/managed-owner-output-vm-target" \
timeout 600 cargo test --locked -j4 --profile test-release -p shards \
  --test managed_output \
  supplied_owner_output_keeps_bytes_ack_cancel_and_neighbor_progress_independent \
  -- --exact --nocapture --test-threads=1

env -u SHARDS_HELPERS CARGO_TARGET_DIR="$HOME/managed-owner-output-vm-target" \
timeout 600 cargo test --locked -j4 --profile test-release -p shards \
  --test managed_output \
  private_owner_output_refuses_wrong_owner_stale_generation_and_cross_member_scope \
  -- --exact --nocapture --test-threads=1
```

Retain environment/source/binary hashes and separate build/test exit logs. First
actual failures are evidence to diagnose, never permission to relax assertions,
increase connection allowance, insert a sleep or discard retirement state.
Independent mutations still need coordinator-owned retained patches and runs:
prematurely advancing consumption in `Service::flush` must break the held-channel
terminal check; removing scope validation must expose actual forbidden bytes to
the rejection observer; auth requires accounting for both TLS and service checks,
rather than calling a still-protected single-check mutation a failing guard.
Byte-routing/cancellation mutations, repeated runs and resource return across
the real daemon/VM remain owed. No mutation result is claimed here.

## R01–R16 checkpoint

| Rule | Evidence / remaining gap |
| --- | --- |
| R01 | Production source unchanged. Assertions are test-only; panic cleanup is caught before shutdown. Selected check, final strict clippy and parser pass as above; actual VM cleanup remains unverified. |
| R02 | Existing daemon metadata/control source, pinned transport and RFC ordered-stream/FIN contract inspected. No production architectural choice is made. |
| R03 | Published capacity and protocol bounds select windows; patience and synthetic values are documented test inputs only. |
| R04 | Existing typed scope/channel/reasons/commands are used; no string control inference. Public Driver TLS-cause erasure remains an explicit limit. |
| R05 | Event waits and protocol deadlines; one metadata/credit authority, no retry/cache/framework. Existing CLI helper polling remains separate debt. |
| R06 | One focused actual client and fixture own four channels and two real variants. No production interface or cross-platform adapter is added. |
| R07 | Comments explain ordering, ownership and limitations; payload content stays out of new diagnostics. |
| R08 | No speed/resource advantage or 4–5× achievement claimed; comparative work is coordinator-owned and unrun. |
| R09 | This adds observation fixtures, not a speculative product fix. Earlier native immediate-refusal red stays recorded separately. VM behavior is unverified. |
| R10 | Light-check commands/source/environment/raw logs/hashes and the corrected lint failure retained. Exact root invocations and guard mutation obligations above. Real VM red/green/mutation/resource logs remain owed. |
| R11 | Initial fixture is Linux-only. All real VM hosts, other six target builds and libc/distribution variants remain unverified; Intel Mac unsupported and paused WinARM untouched. |
| R12 | Public shared transport/identity seams, same existing pinned dependency. No new installed binary, host root or crypto change. |
| R13 | Test/dev-only scope; no production manager, cache, duplicated authority or endpoint-policy change. |
| R14 | Fixture checks local observer descriptor return. Parent/daemon/kernel tasks, bytes, copies, allocation/faults and neighbor resource costs need coordinator measurement. |
| R15 | Newest coordinator-only heavy-work instruction takes precedence. No nested agents, audit/profiling/VM campaign or main merge by implementer. |
| R16 | Checkout was held below30GiB; resumed only after coordinator recovered inactive regenerable cache. Fedora private target is an approved reflink of inactive endpoint target; original preserved and open-FD check empty. No local build. |
