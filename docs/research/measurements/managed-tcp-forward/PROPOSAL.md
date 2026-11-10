# Next experiment: owned bidirectional buffered TCP forwarding

Status: **proposal for ROOT review; no forwarding implementation or native run**.
Source reviewed: `eb5654d6f5356cacaca536ff51f14a632fa7e801`.
Implementer: relay `01a1c5d2ee0e7248982c95a59f1fd905`; coordinator:
`01a148884a4770fdaf0111f3f792a248`. Policy revision `f5cec365`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Canonical AGENTS, full R01–R16, evidence checklist, CLAUDE, managed-runtime plan
and current handoff were read before work and after context loss.

The proposed next fixture forwards one finite, caller-described TCP conversation
through an accepted transparent stream and a distinct owned upstream stream.
It must demonstrate progress in both directions before either participant has
finished sending, then preserve each independent half-close. A byte match from
a fixed responder cannot establish this. ROOT should review the proposed seam,
address limitations and receipt contract below before assigning implementation.

This checkpoint owns only this file, [SOURCE-IDENTITIES.json](SOURCE-IDENTITIES.json)
and [INTERFACE-EXAMPLE.md](INTERFACE-EXAMPLE.md). No production choice, relay code,
kernel/profile/grant change, host administration, extra installed binary, TLS
termination or HTTP proxy is included. Encrypted TCP payloads remain opaque.

## Actual starting evidence

| Established result | Exact scope and remaining distinction |
| --- | --- |
| ROOT r197/r198, `fed0102c8443e46ed9a15a24025eae6fff4c2ddc` | Actual local-to-peer/forward IPv4 TCP reaches the transparent **responder**, with original downstream tuples and distinct backend/relay bytes. Removing the relevant rule yields an actual second backend receipt; restored source passes. [Record](../managed-central-intercept/README.md). |
| ROOT r221, `d5fb2b95e` | Four IPv4 TCP origins, including loopback/self, pass responder checks. This adds no upstream forwarding leg. [Record](../managed-central-followup/README.md). |
| ROOT r531/r556/r567, `7ee6c4bc` / `c141f67d` | Actual finite UDP forwarding has independent backend bytes and a held distinct upstream source; destination-only presentation fails strict checks. Corruption and admission mutations have actual effect witnesses. It does not establish TCP half-close/reset or full server transparency. [Record](../managed-coordinator-forward/README.md). |
| ROOT r597/r626 and closing combined gates | Pending cancellation previously admitted an actual upstream datagram; the narrow existing Deadline correction has positive, guard-removal and restored real effects. Preserve those strict fixtures and receipts unchanged. [Record](../managed-coordinator-boundaries/README.md). |
| New ROOT feedback on `fcc6f85c271b701bfb739511498f88cf15821c34` | Six exact ignored GNU/musl native cases passed, with retained pending/expired tokens, attempts0, owned HUP/ERR classifications, held pidfd exit readiness and parent-FD return. ROOT retains `~/root-managed-native-cancel-fcc6-v1`. These are counter-only admission checks, not external commit proof; the draft is not main-adopted. |

The new TCP fixture remains unimplemented and unrun. No TCP defect is claimed
from the source observations below. Existing notification/postreceipt/extra-UDP
fixtures and their unrun ROOT gates remain preserved on their existing refs.

## Primary contracts and exact implementation context

Identities, Git blob hashes, local SHA256s, retrieval bounds and light-check
results are in [SOURCE-IDENTITIES.json](SOURCE-IDENTITIES.json). Source tags are
research identities, not upgrades. The actual repository
[toolchain file](../../../../rust-toolchain.toml) pins
Rust **1.98.1**; the Cargo MSRV agrees. `libc` is locked to **0.2.189**.
The Rust libc crate declares FFI signatures; it does not pin the GNU C library
or musl implementation installed in a runtime. Those runtime identities remain
part of ROOT's actual artifact/environment record.

TCP closes its directions independently. Graceful forwarding must send all
accepted pending bytes before shutting down the corresponding writer, while
continuing the opposite direction. A reset is an abort, not a graceful EOF.
This is a protocol-derived fixture requirement, not an observed new result.
[RFC9293 §§3.5.3/3.6.1](https://www.rfc-editor.org/rfc/rfc9293.html#section-3.6.1)
(research f9 **B/72**, inference from the standard).

A native send result supplies a local accepted-byte count, not proof the peer
application read it. Nonblocking lack of capacity is an actual EAGAIN/EWOULDBLOCK
result. Preserve the successful prefix before any later error. Reads may be
short; a nonempty stream read returning zero supplies EOF. Never attempt an
empty read and call its zero result EOF. `SHUT_WR` ends only sending.
[send(2)](https://man7.org/linux/man-pages/man2/send.2.html),
[recv(2)](https://man7.org/linux/man-pages/man2/recv.2.html),
[shutdown(2)](https://man7.org/linux/man-pages/man2/shutdown.2.html).

Writability following EINPROGRESS does not establish connection success: inspect
SO_ERROR and require the actual connected peer. The existing `net::tcp` caller
already follows that distinction. `TcpStream::connect_timeout` has its own wait
without an owned cancellation-FD parameter; it cannot supply the proposed
worker's cancellation contract unchanged.
[connect(2)](https://man7.org/linux/man-pages/man2/connect.2.html),
[Rust1.98.1 Unix implementation](https://github.com/rust-lang/rust/blob/1.98.1/library/std/src/sys/net/connection/socket/unix.rs#L167)
(f10 **A/99**, both literal citations checked).

Readiness is permission to attempt I/O, not a byte/EOF receipt. HUP can coexist
with unread bytes. A descriptor with `events = 0` still reports HUP/ERR/NVAL;
exclude a fully inactive leg using the negative-FD convention. Linux6.18.48
`tcp_poll` distinguishes receive shutdown from full HUP and reports socket
errors independently. This supports the proposed interests, but is not an
absence-of-spin proof on the experimental kernel.
[poll(2)](https://man7.org/linux/man-pages/man2/poll.2.html),
[stable TCP source, lines536–650 inspected](https://github.com/gregkh/linux/blob/v6.18.48/net/ipv4/tcp.c#L536).

Rust1.98.1's ordinary TCP `write` uses `send(..., MSG_NOSIGNAL)` on Linux and
returns its native count/error; `flush` does not wait for peer consumption.
Prefer those existing stream operations over a new raw-send/signal policy.
No signal-handler, socket tuning or abortive-linger policy is proposed.
[Rust TCP implementation](https://github.com/rust-lang/rust/blob/1.98.1/library/std/src/sys/net/connection/socket/mod.rs#L432),
[public stream implementation](https://github.com/rust-lang/rust/blob/1.98.1/library/std/src/net/tcp.rs#L699).

Shards pins `hyper-quic` to hyper-raft
`233b7fcef7a363327c5439b191724a38065062cd`, with default features disabled.
It is conformed quinn-proto0.11.18, whose vendored identity points to upstream
`eaec0db4bcb698f76df736d89938743c88a2ad5f`. The actual runtime dependency is
the conformed `hyper-tls`; a separate registry `rustls` lock entry is not its
identity. The older version comment in `remote/Cargo.toml` is not a source pin.
[Pinned manifest](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/Cargo.toml),
[vendor provenance](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/vendor/quinn-proto/VENDORED.md).

QUIC read/finish/reset/STOP_SENDING and stream/connection credit are distinct
contracts. The pinned `write` can accept a prefix; `finish` queues completion,
including its conformed early stream-credit behavior; `reset` abandons pending
transmission. Neither is a managed consumer receipt. RFC9000 distinguishes
received data from data read by the application (f12 **A/90**). Existing
`remote::managed::Endpoint::consumed` checks an application offset separately.
The TCP experiment adds no QUIC payload tunnel or transport-credit policy.
[RFC9000 §§3/4](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.2),
[pinned stream implementation](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/mod.rs#L270),
[local application-credit caller](../../../../crates/remote/src/managed.rs).
QUIC's packet protection and TLS-over-TCP's record protection remain endpoint
responsibilities; this proposal changes neither.
[RFC9001 §3](https://www.rfc-editor.org/rfc/rfc9001.html#section-3).

## Existing callers, ownership and alternatives

All local links below refer to the reviewed base; SOURCE-IDENTITIES freezes them.
The existing host caller `experimental_tcp_interception` in
`crates/shards/tests/managed_network.rs` supplies `intercept-tcp4/6` and artifact
inputs; testguest's main dispatches into `managed_network::main` → parse/run →
`intercept::run` → Worker. Separately, `net-process` calls `shards_net::serve`;
Stack opens/accepts its packet TCP Conns and owns their readiness dispatch.
These are distinct actual caller chains, not two names for the same relay.

| Existing seam/caller | Reusable behavior and reason it is not a complete two-stream pump |
| --- | --- |
| `managed_network::{parse,run,tcp_dial,tcp_serve_on,tcp_send,tcp_receive}` | Native script entrypoints and checked partial-write tails. `tcp_receive` requires the entire expected body **and EOF** before returning; sequencing it before the opposite send deadlocks a server-first or request-EOF-dependent exchange. Preserve existing scripts. [Source](../../../../crates/testguest/src/managed_network.rs). |
| `Deadline::{left,io,cancellation_event}` | Original absolute deadline; non-consuming pending-cancel admission per attempt; EINTR re-admission; native readiness wait; explicit NVAL/ERR/HUP handling; expiry-before-cancel precedence. Its single-stream WouldBlock wait must not monopolize both forwarding directions. No admission rewrite is proposed. Same source, lines214–316. |
| `intercept::{run,prepare,in_namespace,listen,Worker,Completion}` | Held namespaces/listeners, existing fallible worker creation, owned eventfd and Running→Joined owner. Current Worker only receives a fixed request then writes a fixed reply. Its retained report does not describe partial forwarding. TCP's current client is a trusted scoped namespace thread, not UDP's fresh actor child/cgroup. [Source](../../../../crates/testguest/src/managed_network/intercept.rs). |
| `net::Stack::{on_socket,settle}` → `tcp::Conn::{wants_read,wants_write,on_writable,on_readable,flush_to_host}` | Existing interest selection, retained actual-write tails and drain-before-write-shutdown ideas. This is the **host packet TCP stack**, with guest sequence/ACK/window/retransmit/DNS/dialer duties; guest-native accepted/upstream sockets already have kernel TCP state. [Stack](../../../../crates/net/src/lib.rs), [TCP](../../../../crates/net/src/tcp.rs). |
| `net::Ring::{extend,as_slices,drain,release}` → `spare/keep/expire` | Fallible ring growth and two-slice retained tails. Direct reuse also introduces page/power-of-two rounding and the existing thread-local SPARE mapping cache, whose idle expiry is driven by the packet stack. Private `write_now` returns one aggregate count or error; a new fixture needs each successful prefix recorded immediately. No packet-stack defect is inferred. [Ring](../../../../crates/net/src/ring.rs). |
| `net::Poller::{set,forget,wait}` | Established readiness and forget-before-close ownership, with a reused caller event Vec. Direct inclusion adds its poller FD/event storage/platform integration; its existing batch constants are not justified new fixture limits. A fixed native poll set already exists at the guest Deadline seam. [Source](../../../../crates/net/src/poll.rs). |
| `remote::managed::{Producer,Endpoint}` → `net::Io::{write,finish,read_within}` → pinned hyper-quic | Existing managed transport retains partial frame offsets and separate application credit. It is a different byte path and owner; moving guest relay bytes through it would add routing, framing and authority obligations. [Managed](../../../../crates/remote/src/managed.rs), [driver I/O](../../../../crates/remote/src/net.rs). |

Ring's cache has historical data: [M210](../../platform-measurements.md#m210-what-a-network-process-costs-its-host-as-its-connections-grow-d31-e2)
records packet-stack mapping/heap alternatives, retained footprint and a measured
fresh-mapping transfer cost, on named revisions/workloads. That is a reason to
inspect its ownership and costs, not declare it unnecessary. It supplies neither
a one-flow guest buffer allowance nor permission to enlarge a caller's envelope.
No cache is removed or added here. ROOT owns any new allocation/fault measurement.

| Candidate | Benefit | Cost or unresolved obligation |
| --- | --- | --- |
| **Review candidate:** existing Worker owns both native streams; caller-reserved per-direction slots; native joint readiness through the existing Deadline admission | No additional manager, worker pair, transport or retained cache. Retains partial tails and independent half-close states at the guest seam. | Small new private pump still needs actual half-close/error/liveness proof, charged storage and ROOT cost data. A signature check proves only interface compatibility. |
| Import complete `net::tcp::Conn` / Ring / Poller | Reuses mature packet-stack algorithms and existing cost evidence in their original context. | Different TCP endpoint/ACK owner, extra dependencies/state and cache/growth accounting. Not a drop-in stream relay. Do not copy its buffer, batch or retry constants. |
| Two independent copy threads with cloned stream handles | Existing one-FD Deadline waits can block independently. | More workers/handles and joint shutdown/error ownership; clones share a socket. This duplicates concurrency before data shows a need. No extra worker manager proposed. |
| Tokio `copy_bidirectional_with_sizes` | Established Running→ShuttingDown→Done halves; EOF shuts the corresponding writer while the other copy continues (f11 **A/90**). | Adds an async runtime/dependency, cancellation and accounting integration. Default sizes are not an authority source; errors can leave read-but-unwritten bytes. [Pinned prior art](https://github.com/tokio-rs/tokio/blob/tokio-1.48.0/tokio/src/io/util/copy_bidirectional.rs). |
| Envoy native connection/TCP proxy machinery | Established half-close and read/write buffer watermark handling. | Whole dispatcher/filter/buffer/transport machinery is broader than this fixture; half-close is explicit configuration, not a universal default. No timers, watermark numbers or framework imported. [v1.36.0 source](https://github.com/envoyproxy/envoy/blob/v1.36.0/source/common/network/connection_impl.cc#L431). |

This comparison recommends a **bounded experiment**, not production adoption.
There is no measured 4–5× result, overhead allowance or chosen production capacity.

## Interception and native address contract

Current `wire::interception_messages` installs a PREROUTING TCP selector,
TPROXY/mark and a distinct fallback DROP. For `Origin::Forward` it installs no
OUTPUT chain; other origins add OUTPUT marking. Thus an upstream socket created
in the existing owned router namespace is a source-supported **Forward IPv4
candidate** for avoiding re-interception of its locally originated leg. ROOT
must still prove the actual route and tuples. Do not invent an exemption mark.
[Current emitted rules](../../../../crates/testguest/src/managed_network/intercept/wire.rs),
[Linux6.18.48 TPROXY implementation](https://github.com/gregkh/linux/blob/v6.18.48/net/netfilter/nft_tproxy.c).

TPROXY preserves the destination without NAT and needs transparent sockets plus
policy routing. Its hook/type restrictions do not establish every route. A
REDIRECT substitute changes destination handling and has TCP destination-recovery
limitations; it is not a free equivalent.
[Kernel contract](https://github.com/torvalds/linux/blob/v6.18/Documentation/networking/tproxy.rst).
SOCKADDR is a separate comparison: Linux's connect4 selftest demonstrates address
rewrite/bind capability, but Shards' current emitter explicitly selects
SOCK_DGRAM/IPPROTO_UDP. Its present Connect4/Getpeername4 names do not prove a TCP
program or original TCP address presentation. Extending hooks needs a separate
review and actual evidence; it is not part of this proposal's initial route.
[Shards emitter](../../../../crates/testguest/src/managed_network/intercept/sock_addr/bpf.rs),
[kernel selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/connect4_prog.c).

For each actual mediated conversation the strict record must establish:

- Client `peer_addr == original_destination`; accepted relay `local_addr ==
  original_destination` and `peer_addr == actual_client.local_addr`.
- Upstream `peer_addr == original_destination`; backend `local_addr ==
  original_destination` and `peer_addr == held_upstream.local_addr`, distinct
  from the held actual client source. Hold baseline and upstream descriptors
  while correlating tuples to avoid source-port-reuse ambiguity.
- Namespace identities come from supervisor-held objects; a supplied payload,
  reported tuple, flow ID or mark cannot select authority. No backend FD escapes
  to the client. Current trusted fixture roles do not prove hostile-principal
  attribution or production grant admission.

The initial backend therefore sees the owned upstream source, as in the UDP
experiment. **Full native backend source presentation is unresolved**, not
waived. Local/loopback/self OUTPUT re-interception, IPv6 forwarding, wildcard
listeners, peer/engine and external ingress/egress still need mechanisms and
actual return-path evidence. DNS, UDP/QUIC, Unix pathname/abstract/socketpair,
SCM_RIGHTS, unsupported families, policy replacement and restored generations
remain mandatory plan work. A one-flow success closes none of G04–G07/P4–P6 as
a whole; an unsupported route must never receive direct fallback permission.

## Proposed smallest state and signal change

After review, put private forwarding logic in one new guest module and select
it through the existing Worker. The same Worker owns cancel, both stream FDs,
the two reserved slots and one JoinHandle. Retain the current responder entrypoint
and assertions. The backend script runs in its existing independent worker;
there is no forwarding manager, cache or periodic scheduler.

Prepare the **unconnected** native upstream socket with existing `bound` inside
`in_namespace(router, ...)`, then move that owned FD into the worker. Creating
it in a thread merely holding a router listener would not put a new socket in
that namespace. Convert the FD to TcpStream and perform one nonblocking connect
to the actual bound backend destination. No arbitrary source port, transparent
spoofed upstream bind, SYN-data mode, socket tuning or redial is proposed.

| State | Actual event and legal transition |
| --- | --- |
| Prepared | Checked caller envelopes, original `at`, reserved payload/control storage, owned listener/upstream FD; actor still held. Setup failure returns retained acquisition/cleanup facts. |
| Accepting | Actual accept → DownstreamAccepted with actual tuple. Missing listener/extra connection is a typed failure, not a permission to bypass. |
| DownstreamAccepted / ConnectPending | One actual connect: success or EINPROGRESS. Joint readiness then actual SO_ERROR/known peer → Forwarding; native failure → terminal Native. An interrupted connect is retained as uncertain and ended without replay. |
| Per direction: Reading | Nonempty native read n>0 adds to retained tail and checked read count; actual WouldBlock waits for interests. A full slot disables only that source read. |
| Per direction: DrainingAfterEof | Actual nonempty read0 records source EOF. No further source reads; pending bytes continue writing. Opposite direction remains active. |
| Per direction: WriteClosed | Only after that tail is empty, successful native Shutdown::Write on the destination records write closure. It does not imply peer application receipt. |
| Clean (worker-local) | Both directions reached WriteClosed with both actual source EOF witnesses and no pending bytes. Worker returns its completion; cleanup wake cannot erase confirmed facts or manufacture Clean. Independent endpoint receipts are still required for the host's strict pass; this local terminal does not prove delivery. |
| Cancelled / Expired / Native / ControlFailure / Limit / UnexpectedConnection | Separate typed terminal causes with stage/leg/operation and native errno where present. Preserve all accepted/read/written/pending/EOF/shutdown facts; cancellation is not rollback. Then stop and join acquired owners. |

Each direction maintains `read_bytes = kernel_written_bytes + pending_bytes`
until abort; an abort retains the pending count even when its storage is later
released. Counters advance immediately after each actual successful call, before
the next admission check. This avoids turning a later error into a claim that
the earlier prefix never happened. Independent endpoint receipts remain separate.
Do not issue empty writes. An actual nonempty write0 is typed WriteZero with the
tail retained; it is neither progress nor a reason to repeat without readiness.
Validate returned counts before advancing a checked tail.
Report typed stages Accept/Connect/ClientRead/BackendWrite/BackendRead/ClientWrite/
ShutdownBackendWrite/ShutdownClientWrite/Wait; do not decode cause from error text.

The checked [interface example](INTERFACE-EXAMPLE.md) shows the existing generic
Deadline return type accepts `Attempt::{Ready,WouldBlock,NativeError}`. A proposed
closure maps **real** WouldBlock to a typed result, so the owner can service the
other stream rather than entering the old one-FD wait. Read/write/poll EINTR
returns through Deadline re-admission. Connect is handled separately as above.
No synthetic WouldBlock and no cancellation-disabled forwarding path.

One native readiness wait covers active interests of the two streams and the
owned cancel FD, using the **same original absolute deadline**. Each wait attempt
recomputes remaining time; expiry then cancel classification follow the existing
precedence. Native NVAL is failure; HUP/ERR require actual socket/error/EOF
observations rather than fabricated success. Rebuild interests after actual
progress; stop reading an EOF source and requesting writable when no bytes remain.
ROOT must exercise unread-data+HUP, full-tail+half-close and spurious/no-progress
readiness before any liveness claim. A source model is not such a proof; do not
mask a reproduced loop with a timer, guessed retry count or cached ready bit.

Each actual attempt still incurs the existing non-consuming cancel-readiness
syscall. ROOT owes its induced cost. Pending-before-attempt cancellation is the
intended admission guarantee. Cancel-after-check races remain; no atomic external
commit, rollback, linearizability or universal prompt-cancellation claim follows.
Test-only barrier waits, if subsequently approved, keep explicit `cancel=None`
only in their trusted control path with the same `at`, never on forwarding I/O.

## Finite caller input, concrete source/harness paths

Caller supplies original operation duration, separate boot/outer deadline, exact
opaque request/greeting/tail bytes and lengths, per-direction pending-byte and
control-record envelopes, exactly one scripted flow, and existing fixture
mark/table/priority. All additions/conversions and fallible reservations occur
before namespace/listener/actor effects. Account actual reserved capacities;
reject excess without increasing authority. No buffer growth during forwarding.
Payload slots do not bound allocator retention, kernel socket queues, FD/thread
cost, namespace/netlink work or aggregate parent charges; ROOT/P1 evidence is owed.

The cumulative byte envelope must leave a separately charged, minimum nonempty
EOF check at an exact script boundary, following the existing endpoint helper's
distinction. A zero-length read cannot establish EOF. If that check consumes an
extra byte, record its actual value/offset and a typed UnexpectedData terminal;
do not forward it or hide it in a zero-byte result. Keep unexpected-consumed bytes
separate from the admitted forwarding prefix in the retained accounting. This
test-only observation storage derives from distinguishing EOF from an extra byte,
not a production queue-size choice.

The script is finite and can exceed a pending slot's size; backpressure must
stream it rather than accumulate an entire message. Supplied bytes/times are test
inputs, not production constants. OS selects ports. The existing experimental
kernel and effective config are supplied and hashed; no product kernel pin changes.

| Future path — requires a separate implementation assignment | Minimal responsibility |
| --- | --- |
| NEW `crates/testguest/src/managed_network/intercept/tcp_forward.rs` | Private one-flow procedure, typed half/terminal reports, retained tails, native connect and joint readiness. Receive already owned sockets/slots/deadline; no manager. |
| `crates/testguest/src/managed_network/intercept.rs` | Narrow work/report selection in existing Worker; prepare router-owned upstream FD and finite endpoint scripts. ROOT must assign this currently shared source scope before edits. Existing responder behavior stays covered separately. |
| `crates/testguest/src/managed_network.rs` | Parser/dispatch only for proposed `intercept-tcp-forward4`; reuse Deadline unchanged. New entrypoint must not relax existing TCP/UDP/cancel strict fixtures. |
| NEW `crates/shards/tests/managed_network/tcp_forward.rs` | Independent typed transcript/parser assertions, actual opposite-end bytes/EOF/tuples, cleanup facts. ROOT wires shared host parent/helpers/header. |
| Future `docs/research/measurements/managed-tcp-forward/run.sh` | Finite ROOT-owned driver: supplied artifact hashes/config/inputs, exact filters/outcomes, raw receipts, meaningful mutations and finally exact restoration. **Not created by this proposal.** |

Use separate fresh cases and the existing baseline/held tuple. The new forward
case expects backend **two** connections: baseline and the owned upstream leg;
relay **one**. Existing responder cases still expect backend1/relay1 unchanged.
Each backend receipt names its connection ordinal and actual peer, preventing a
direct client connection carrying correct bytes from masquerading as forwarding.

## Decisive ROOT native/backend receipts before adoption

Proposed finite native endpoint actions are controlled by private one-shot
handshakes, not sleeps. They carry no actor-provided authority. Record each
actual action/result before releasing the next dependent action. A missing or
duplicate control notification is typed failure under the original deadline.

| Case | Required independent witness; pass cannot be inferred from relay counts |
| --- | --- |
| Server-first then client FIN | Backend sends supplied greeting **before client write closure**; client actually reads it while its write half remains open. Client sends request then SHUT_WR. Backend actually reads exact request and nonempty-read0, then sends final tail and SHUT_WR. Client actually reads exact greeting+tail and read0. Both worker direction closures and tuple equalities hold. |
| Backend FIN first | Backend sends declared prefix then SHUT_WR while retaining its reader. Client receives prefix+EOF, then sends the declared request and SHUT_WR. Backend receives that request+EOF **after its own write closure**. Returning on first EOF or Shutdown::Both must fail this case. |
| Partial write / controlled slow reader | Caller supplies finite payloads and envelopes; backend holds reads behind a private release. Record actual n<offered and/or native WouldBlock, exact retained tails and eventual independent bytes. If the kernel accepts every offered write, this is **unobserved partial-write coverage**, not a pass of that boundary. No picked SO_SNDBUF/size/interval to make it appear. |
| Backend reset after receipt | Backend records an exact prefix before an owned reset action; worker records actual native reset/error and retained prefix facts. Client records its real terminal outcome. Clean/EOF cannot substitute for required reset semantics. Dropping the other stream does not automatically forward an RST; exact propagation remains a gate. A red needs actual logs before any scoped remedy; no global abortive-linger change is approved. |
| Pending cancellation, then cancellation after a prefix | Preserve original strict pre-send no-effect and independent unexpected-backend receipt on red. For a committed-prefix case retain actual backend bytes and written/pending facts. Expiry is separate; original remaining time is reported. No rollback or cancel-after-check atomicity claim. Existing UDP pending/receive/normal controls stay intact. |
| Error/early finish/extra connection | Backend actual bytes and native terminal cause identify the exact connection even if completion arrives before/after a control notification. Unexpected accepts/extra stream bytes are finite strict failures. Omit/duplicate notifications cannot become clean finish or cancellation. |
| Cleanup on every terminal stage | Join every acquired worker and namespace helper; close actual owned downstream/upstream/listener/cancel/control/netlink/namespace FDs. Record each acquired object's cleanup result and uncertainty on error. The current TCP client is a trusted namespace thread: child reaping/cgroup removal cannot be claimed without separately approved owned-child scope. ROOT also checks enclosing VM/host descriptor return. |

TCP connection establishment is itself an effect. A pending-cancel check before
the first payload write cannot erase an earlier SYN/backend accept. Record that
connection separately from payload receipts; specify whether a future cancellation
gate holds before connect or before a payload attempt. Retain the existing UDP
strict pre-send assertions unchanged. Any unexpected TCP payload send needs the
independent backend's actual connection/offset/byte receipt on red, not a missing
alert or a claim that zero local writes undid an earlier connection.

Receipts need role, connection ordinal, namespace identity, actual local/peer,
byte offset+length+actual fixture bytes, native operation/result and per-half
terminal. Store trusted test bytes only; no private TLS content or credentials
enter ordinary telemetry. Preserve raw guest/stdout/stderr and frozen executed
artifacts with hashes and source-matching normal carried helpers. A read receipt
proves that endpoint consumed the named test bytes, not an application side effect.

ROOT's meaningful mutations should remove only the forward interception rule
(actual direct backend peer/receipt must fail even with correct client bytes),
corrupt a retained request/reply byte (the opposite endpoint records corruption),
and omit drain-before-FIN or the needed write-half close (actual tail/EOF witness
fails). ROOT owns execution and mutation/restoration. Finally restore exact
source SHA plus empty diff, rebuild matching helpers and repeat clean controls.
No missing-alert-only verdict. New failures are not authorized speculative fixes.

## Validation, owed work and policy accounting

The private signature-only GNU/musl checks and mechanical policy check pass;
the first private layout failure is retained. No native fixture/test operation
ran. This is not a normal host/helper build, whole-workspace lint, seven-target
gate, VM gate, liveness/cleanup proof or performance result. Detailed identities,
commands and logs are in [SOURCE-IDENTITIES.json](SOURCE-IDENTITIES.json).

| Rule | Evidence in this docs checkpoint / remaining obligation |
| --- | --- |
| R01 | No runtime implementation. Actual source and function-pointer appendix type-check; future checked lengths/arithmetic/reservation/error paths need review and tests. No universal panic-free claim. |
| R02 | Pinned primary standards/source, existing actual prerequisites and established alternatives above. Only experiment recommendation; production mechanism/resource decision awaits actual data. |
| R03 | No production number selected. One flow/two directions and poll-set entries derive from fixture topology; supplied budgets and ABI time units do not create production allowances. |
| R04 | Explicit half, connect, terminal and owned-join transitions; retained effect facts, typed native/control/expiry/cancel distinction specified, not implemented. |
| R05 | Existing ownership/readiness seams; wait/cache comparison includes M210 context. No new manager/cache/timer. Liveness and per-attempt syscall cost remain ROOT work. |
| R06 | One private procedure and narrow existing caller wiring proposed; no generic framework or arbitrary complexity threshold. Code review remains owed after approval. |
| R07 | Source citations and non-obvious ownership/half-close rationale; no runtime comments changed. |
| R08 | No comparative result. ROOT must select equivalent pinned native baselines/workloads/configs and report ratios and misses against the 4–5× target. |
| R09 | No speculative bug fix. Actual UDP/native feedback remains revision-scoped; any new TCP remedy needs its own live repeatable red. |
| R10 | Durable proposal/identities/interface check, retained failed checker layout, explicit future actual-effect and mutation/restoration gates. No unrun native check counted as passed. |
| R11 | GNU/musl **signature checks only** on Linux amd64; other targets and actual KVM/HVF/WHP source-matching gates are unverified here. macOS ARM64 only; Windows ARM VM stays paused. |
| R12 | Existing Linux guest namespace/intercept seam and shared host reporting; no extra installed artifact, host root, OS-specific parallel manager or crypto change. |
| R13 | Small complete one-flow experiment proposed; full mandatory route/native semantics/resource obligations remain open. |
| R14 | No agent profile/benchmark. ROOT owes allocations, reallocation/copies, retained/parent/kernel cost, faults, CPU and latency/throughput distributions with uncertainty. |
| R15 | Implementer docs and admitted pure signature checks only. Signed named-path branch checkpoint; ROOT reviews/assigns code, runs all actual campaigns and merges. No children/auto dispatch or lane-closure claim. |
| R16 | Existing worktree reused; no local target/cache or heavy build/new full copy; new private remote source/target/cargo-home capped to one CPU/1GiB with a 2GiB disk admission, retained without deletion. ROOT protected paths untouched. |

Next authority boundary: ROOT reviews this separate proposal and assigns any
approved code paths/cases. Until then the work remains research and checked
interface documentation. Managed-runtime closure and lane release remain ROOT's
decision after actual combined evidence.
