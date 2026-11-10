# Finite owned UDP relay cancellation fixture

Implementer source starts at exact main/origin/main
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`, branch
`p/adversarial-relay-cancel`, private worktree `managed-relay-cancel`.
POLICY_READ implementer: full AGENTS, R01–R16, evidence checklist, CLAUDE,
managed-runtime plan and current `/Users/adalundhe/g121-handoff.md` read before
edits. Policy file revision `f5cec365`; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root approved the concrete finite synchronization proposal before implementation.
Root owns all audits, actual socket/namespace/cgroup/BPF/VM execution, adversarial
campaigns/mutations, profiling, benchmarking, combined gates and main merges.

This prepares a previously unverified cancellation boundary. It is **not a
reproduced product defect or a runtime pass**. Root's actual baseline/r556 guard
and restoration results remain in the [forwarding record](../managed-coordinator-forward/README.md).
Ordinary scripted and buffered commands remain available with the same absolute
caller deadline and original endpoint predicates. No production feature, grant,
policy, kernel, source-port preservation or actor control authority is added.

## Contract and concrete states

New explicit coordinator command, with no deadline/buffer default:

```
sock-addr-cancel4 {connected|unconnected} WORK_MS BUFFER_BYTES
```

It selects the existing FixedOriginalPeer hook plan. Reservation still precedes
namespace/cgroup/socket acquisition. The actual baseline remains Binary then
Empty through a fresh exec child. The same backend worker's explicit ReplyPlan
moves Baseline → AwaitForwardedBinary after the baseline Empty reply. It then
records/validates the next actual Binary backend receipt, enters HoldReply and
signals one receipt eventfd. It issues no reply for that held request. An actual
extra datagram is consumed, recorded as BackendUnexpected and returns typed
UnexpectedDatagram, rather than spinning on readable readiness.

The existing forward worker records ReceiveStageEntered after the full upstream
send and before its existing UpstreamReceive decoder call, then signals a separate
one-shot eventfd. Both new eventfds are nonblocking/CLOEXEC; the actor receives
neither after exec. The parent waits for both notifications with the **original
absolute Deadline.io** and then requests cancellation through the existing
DatagramWorker cancellation eventfd. Notification reads consume only the two
boundary events; cancellation detection remains in the existing worker path.
There are no sleeps, periodic probes, caches, decoder copies or second worker
manager. Notification values must be exactly one uint64 event, otherwise InvalidInput.

This observes receive-stage entry, **not the instant a kernel thread sleeps**.
The decisive forward result must be Binary / UpstreamReceive / KernelAccepted /
Cancelled with ingress_received=1, upstream_sent=1 and upstream_received=0,
client_sent=0, empty_ingress=0, empty_reply=0. Existing cancellation classification
requires ConnectionAborted from Deadline.io and the existing eventfd observation;
TimedOut remains Error and cannot satisfy that predicate. Already consumed/sent
facts survive failure/cancellation. A send return alone still does not prove
backend commitment; the independently recorded backend receipt supplies that fact.
Cancellation does not roll back this receipt.

The existing OwnedClient is retained. Its former combined exec/wait body is split
into start_client and RunningClient::finish so the parent can cancel while its
fresh child waits. The old exec_client wrapper still waits naturally. The new
case explicitly kills and reaps only this owned waiting child, then follows the
existing worker join → every selected link detach → cgroup removal path. The
actual SIGKILL termination is recorded, not inferred from a request. After both
worker joins the original Deadline.left must still succeed and supplies positive
remaining nanoseconds. Backend completion must be HoldReply / Cancelled with
three actual receipts, three validations, one Empty and only the two baseline
replies sent. Cleanup failure is still failure. Existing unknown child-reap
handling remains bounded by the outer coordinator VM lifetime; this fixture is
not a general population/kernel-return proof.

The new host module `crates/shards/tests/managed_network/cancel.rs` reuses the
parent's Inputs::supplied/boot, record parser, identity and backend-source helper.
It uses the refactor lane's exact `assert_sock_addr_hooks_and_cleanup` helper
(source `e1637959af1dbae631aa6f91c0d4d227f4c7dfac`), preserving independent actual
backend/ingress bytes, client namespace/cgroup/original tuple, both child reaps,
held/no-reply state, stage/terminal/deadline and cleanup assertions. Root must
compose that parent change and add only this declaration to the parent:

```rust
#[path = "managed_network/cancel.rs"]
mod cancel;
```

This implementer did not edit host `managed_network.rs`. The module is ignored
for real VM execution. Missing/extra/mismatched facts fail; no child response,
ForwardUpstreamReply, ForwardExchange or BackendUnexpected is accepted.

## Basis, alternatives and literals

The [existing buffered source decision](../managed-network-sock-addr/buffered-relay/README.md)
retains pinned Envoy v1.36 UDP proxy/Cilium v1.18 socket-address alternatives,
Linux UDP/send/receive contracts and the unchanged existing worker/decoder seam.
Root's r556 actual forwarding and meaningful guard effects justify exercising
this owned worker boundary; they do not establish the new cancellation result.

Primary sources inspected for the added synchronization are Linux **v6.18**
[fs/eventfd.c](https://github.com/torvalds/linux/blob/v6.18/fs/eventfd.c) and
[fs/select.c](https://github.com/torvalds/linux/blob/v6.18/fs/select.c), plus
[eventfd(2)](https://man7.org/linux/man-pages/man2/eventfd.2.html).
The kernel's eventfd read/write/poll contract retains notifications that precede
waits and shares the existing poll deadline/cancel mechanism. Retrieved byte
identities are in [primary-identities.json](primary-identities.json).

Alternatives considered: a pipe notification needs two descriptors per boundary;
a condition variable or channel would require a separate wait/deadline path to
compose socket cancellation. A thread sleep or observing a counter repeatedly
would not establish the required boundary. Reusing the existing eventfd/Deadline
seam adds two finite notification objects and their owned duplicates, with no
shared user-memory state or new platform adapter. This is a scoped test choice,
not selection of the production relay/accounting architecture.

New integer literals derive from the declared finite scenario: one held Binary
request, one notification per boundary, two signals for independent facts, the
existing Binary+Empty baseline and exact script lengths. Eventfd words use its
uint64 ABI; guest truncation flags and SIGKILL=9 are Linux ABI facts. Ports remain
OS-assigned. WORK_MS and BUFFER_BYTES remain caller inputs; overflow/allocation/
capacity failure refuses, and no cleanup grace extends the work deadline.
PayloadCapacityOnly still excludes allocator/parent/kernel/socket/console costs.

## Actual light results

All own jobs completed. Fedora amd64 runner `8fadc1ec213332e16cb0d915a3fb913f`
admitted **1 CPU / 1 GiB / 2 GiB** via Kiln headroom; rustc1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`. NEW private source/target directories;
no protected root source/cache writes. Mac free at start34379940KiB (>30GiB),
no local build, no Windows ARM access or resume.

| Implementer job | Actual result |
| --- | --- |
| r1, `~/managed-relay-cancel-proof-v1` | 32 GNU pure tests0;32 musl pure tests0;both scoped production panic/unsafe lint subsets0. No sockets/ns/cgroup/BPF/guest cases run. |
| r2, `~/managed-relay-cancel-host-v1` | Actual sibling parent + our module compile composition with inert type-matching common stubs; all ignored bodies compile0; new expiry/wrong-stage witness and three existing exact pure filters each1/pass1. No VM helper executes. |
| r3 | Text-only evidence pack;34 entries/33 member hashes independently verified locally; no executable bytes transferred. |

Replay (NEW private directories, admitted envelope unchanged):

```sh
python3 docs/research/measurements/managed-relay-cancel/light.py guest "$HOME/managed-relay-cancel-proof-NEW"
python3 docs/research/measurements/managed-relay-cancel/light.py host "$HOME/managed-relay-cancel-host-NEW"
```

The wrapper reuses existing source-linked pure/light harnesses with `-j1`; no
second host VM harness. It composes sibling parent source plus module declaration
only in the copied host fixture. [light/](light/) retains exact per-command JSON,
stdout, Cargo locks, scaffolding, source manifests and binary identities. Guest
sources match signed paths; [source-identities.json](source-identities.json)
identifies this delta. Three actual test binary hashes are retained remotely and
listed in [light/binary-identities.json](light/binary-identities.json). Archive
7212B SHA256`2b674f6a2016488c19814252e7e3e541924a2114bce15811b597adf984dcdc78`;
original private source/targets/binaries/archive remain preserved. These are
scoped lints and inert host composition, not full actual helper/workspace/seven
target or VM validation. Local owned rustfmt, policy and source/docs diff checks pass. Staged raw
Cargo output has six unchanged terminal-blank-line warnings from diff --check;
all six log bytes remain exactly member-hash verified, with no global/source
whitespace-rule relaxation.

## Owed coordinator validation

Root composes the shared assertion parent and module declaration, then builds
actual normal host/guest/helpers. With the same explicitly supplied experimental
kernel/config/identity environment as the forwarding record:

```sh
SHARDS_SOCK_ADDR_MODE=connected \
SHARDS_SOCK_ADDR_BUFFER_BYTES=18 \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  cancel::direct_guest_udp4_cancels_after_backend_receipt_before_work_deadline \
  -- --ignored --exact --nocapture --test-threads=1
```

Repeat unconnected with a fresh records directory.18 is the existing concrete
9-byte request+9-byte reply boundary, not a runtime default. Retain exact source,
host/guest/helper/kernel/config hashes and actual stdout/command/status before
assertions. The original WORK_MS remains the only work deadline, and positive
remaining time after joins is mandatory.

Root owes actual receipt/receive-entry→cancel→child reap→worker joins→link detach→
cgroup removal; removal of cancellation handling must fail on typed expiry/error,
not be accepted as Cancelled, then exact source restoration must pass. Also owe
work-deadline expiry, notification/record/OS failure, extra datagram and child/
worker failure cases; original connected/unconnected relay and responder plus
routing/presentation/capacity/buffer mutation controls; full combined matrix and
actual supported KVM/HVF/WHP coverage. No real runtime, cleanup-time, hard budget,
allocation/page-fault, neighbor fairness or performance outcome is claimed here.

## R01–R16 evidence and remaining gaps

| Rule | Delta evidence / owed root work |
| --- | --- |
| R01 | Fallible eventfd/clone/spawn/I/O, checked counts/slices/capacity; existing denied production panic lints retained; GNU/musl scoped lints0. Actual OOM/OS/error paths unverified. |
| R02 | Approved finite proposal; pinned Linux eventfd/poll and linked Envoy/Cilium alternatives; actual prior baseline linked. New runtime behavior owed. |
| R03 | Scenario/script and eventfd/Linux ABI derivations above; caller deadline/envelope, OS ports; no learned authority/default/cleanup extension. |
| R04 | ControllerKind, ReplyPlan, HoldReply, receive stage, ChildEnd, existing worker Running→Joined and typed Fault/Terminal remain explicit. Expiry/wrong-stage pure negatives pass; actual transitions owed. |
| R05 | Existing eventfd/Deadline/decoder/worker/child seam; two one-shot notifications; no cache/poll loop/second manager. Unexpected datagram terminates. Actual waits/cleanup owed. |
| R06 | One optional finite boundary behavior and split existing child lifecycle; no generalized flow engine. Root source/evidence review still required. |
| R07 | Comments identify notification ownership, no kernel-sleep observation, extra-readiness failure and original cleanup ordering; no rollback claim. |
| R08 | No benchmark/comparative4–5× claim; root comparative distributions remain open. |
| R09 | Authorized new coverage for an explicit GAP, not a claimed/fixed runtime defect. Root exact reproduction required for any later fix. |
| R10 | Source hashes, exact light commands/logs/binaries and primary identities retained; actual baseline/mutation/restoration for this boundary owed. |
| R11 | GNU/musl amd64 pure/light only. Full seven-target/real hardware coverage owed; macOS arm64 only, Windows ARM pause preserved. |
| R12 | Existing native guest/socket/cancel/worker/child mechanisms; no installed helper/platform/grant/crypto/kernel changes. Product architecture remains undecided. |
| R13 | One finite existing flow, two independent facts, shared strict host assertions; original fixtures retained. No source-port trick or broadened authority. |
| R14 | No implementer profiling/benchmarking/audit. CPU, allocation/copy/fault/retention/kernel/parent costs remain root-owned and unmeasured. |
| R15 | Implementer scoped files/pure checks only; no children/heavy/adversarial campaigns/main merge. Signed named-path checkpoint and exact owed root runs returned. |
| R16 | NEW private source/targets, admitted1CPU/1GiB/2GiB; root caches/VMs/evidence untouched; Mac floor observed, no deletion or headroom bypass. |
