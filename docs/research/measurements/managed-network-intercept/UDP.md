# Peer UDP interception experiment — source/light checkpoint only

This TEST-ONLY extension is not an adopted product mechanism. Source checkpoint
`30eaaee9c15aebcfe3e0c9a81b73571b134a074c`, explicit-state correction
`9df26e1f141379fc6207aa0500b911bb06613fc7`, base
`aa6fc4f29390f3a422474071db92e1448071e207`. The coordinator approved the narrow
peer proposal before implementation and reviewed the first source checkpoint.
**No actual UDP socket, namespace, guest, guard mutation, resource or performance
campaign ran in this implementer lane.** Actual effects remain coordinator gates.

## Identity and scope

POLICY_READ implementer: canonical `g121-main/AGENTS.md`, full R01–R16,
`development-evidence.md`, `CLAUDE.md`, managed-runtime plan and current handoff;
revision `f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root owns all audits, profiling, benchmarking, heavy E2E, chaos, sweeps and merges.
No nested agents, root clone/cache writes, host administration, product routes,
kernel/config changes, new installed binary or crypto changes.

Finite inputs: `intercept-udp4`/`intercept-udp6`, `local`/`forward`, and
`connected`/`unconnected`. Each actual guest is a fresh three-namespace topology
using the existing RFC5737/RFC3849 fixture addresses. Both phases perform the
explicit binary then empty exchange; UDP connect selects a default peer and does
not establish a TCP-like connection. The backend port and client/receiver ports
come from actual bound sockets. Mark/table/priority and the single absolute
setup/socket deadline remain explicit checked caller inputs.

The experiment is a transparent **responder**, not a forwarding relay. It never
connects to, forwards bytes to, or transfers a backend socket to the actor.
Loopback/own-address UDP are required follow-ups: the proposed same-namespace
reply bind would conflict with the existing native backend tuple. This is a
source/design constraint, not a reproduced bind failure. Multi-destination UDP,
IPv6 extensions/fragments/mapped/link-local/scope cases, fragmentation/PMTU,
QUIC, real forwarding, full route coverage and UDP relay-absence hard guarantees
remain unverified. The UDP parser deliberately rejects the existing TCP local
and absent-listener variants; no TCP deadline containment verdict is borrowed.

## Primary contracts and alternatives

All Linux links below are pinned to v6.18; the coordinator's experimental runtime
kernel is a separate built artifact and still needs actual UDP validation.

- [Linux UDP header](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/udp.h#L21-L26)
  and [RFC768](https://www.rfc-editor.org/info/rfc768/) establish datagram boundaries,
  protocol17 and the destination-port field at transport offset2. No reliability,
  duplicate protection or ordered stream semantics are inferred.
- [IPv4 original destination](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/ip_sockglue.c#L139-L157)
  and [IPv6 original destination](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/datagram.c#L678-L695)
  construct sockaddr ancillary records from the packet destination address and
  destination port. The fixture requires both to match the authoritative backend.
- [recvmsg, man-pages6.19](https://man7.org/linux/man-pages/man2/recvmsg.2.html)
  specifies zero-length datagrams, input MSG_TRUNC's reported full datagram length,
  and output MSG_TRUNC/MSG_CTRUNC. Zero is a consumed datagram, never stream EOF.
- [IPv4 UDP send](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/udp.c#L1049-L1068)
  and [IPv6 UDP send](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/udp.c#L1283-L1301)
  source the UDP port from the sending socket/flow. Same-socket receiver replies
  would use its redirected port, violating native client source-tuple semantics.
- [TPROXY IPv4 lookup](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/netfilter/nf_tproxy_ipv4.c#L111-L127)
  and [IPv6 lookup](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/netfilter/nf_tproxy_ipv6.c#L112-L127)
  exclude unconnected UDP sockets from established lookup. Reserve one transparent
  **unconnected** reply socket at the backend tuple in the NEW router before
  wildcard receiver allocation, preventing a receiver-port collision. Do not
  connect it and accidentally create a different lookup candidate. This reasoning
  requires actual packet/guard validation; source is not a runtime pass.
- [nft_tproxy](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_tproxy.c)
  supports TCP/UDP at PREROUTING and returns NFT_BREAK without a suitable socket.
  Preserve the existing separate following exact-selector DROP, not blanket
  base-chain denial. UDP adds only the real L4 selector variant; policy routes,
  OUTPUT marking, grants and unmatched traffic behavior are unchanged.
- [Pinned kernel TCP selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/net/netfilter/nft_tproxy_tcp.sh)
  provides the local/forward topology and distinct responder witnesses, not UDP
  evidence. Its shell/socat/process probes and numeric timeouts are not copied.
- [TPROXY documentation](https://github.com/torvalds/linux/blob/v6.18/Documentation/networking/tproxy.rst)
  requires transparent nonlocal binding and policy routing, and describes NAT's
  destination modification. NAT is not introduced to manufacture parity.
  [Envoy v1.36.0 sendmsg](https://github.com/envoyproxy/envoy/blob/v1.36.0/source/common/network/io_socket_handle_impl.cc#L138-L210)
  is the inspected established per-message source-IP alternative. It requires
  ancillary send construction and still relies on the socket's source port.
  Reuse-port sharing, raw packet construction and NAT add semantics/authority not
  justified by this peer-only experiment, so none is added. No comparative cost or
  product mechanism selection follows from this source comparison.

Direct curl attempts during proposal research returned429, and an obsolete
selftest URL returned404; the primary web reads at the correct paths above
succeeded. Neither retrieval failure is a network/runtime reproduction.

## State, observation and ownership

The existing native strict `udp_receive` wrapper keeps its length/flags, payload,
then metadata validation order and corresponding errors. Its actual recvmsg is
factored once: `UdpArrival` retains reported length, copied prefix, flags and
metadata validity even when validation fails. A successful recvmsg increments
actual arrivals before validation or output; exact matched and empty counts are
separate observations. Truncated/incorrect packets cannot become zero delivery.
The native wrapper still rejects them; the fixture preserves raw facts first.

Worker protocol state is an explicit `Exchange`: Binary→Empty after the reply;
Empty→Binary for the retained backend receiver, or Empty→ScriptComplete for the
relay. Arrival count is observation-only. The first checkpoint inferred exchange
from count modulo script length; the coordinator requested the R04 correction.
Both versions/raw results are preserved. This is a conformity correction, not a
new live UDP defect claim. Clients iterate the declared finite script.

Stages distinguish bind, default-peer selection, datagram send, receive,
validation, reply send and recording. Errors retain typed cause/stage and relevant
I/O kind/errno. Every arrival records actual local/peer/original tuple and copied
hex. Binary server roles reply with different BACKEND/RELAY bytes; empty packets
have independent zero-length receiver/tuple observations and cannot identify a
role by empty bytes alone. The client requires the original backend reply source,
including port, and original-destination metadata naming its own bound endpoint.

Each worker owns its sockets, eventfd and join handle. Cancel wakes existing
readiness waits; original absolute deadline also bounds cleanup. A cancellation
terminal requires an actual readable eventfd after the wait's cancellation error.
Every started worker is joined before propagating setup/client/worker failure;
completion retains counts even on I/O errors. Baseline FD stays held through the
fresh intercepted call, preventing source-port reuse. No readiness probes,
periodic polling, application retries, flow cache or new relay manager are added.
New allocations use existing fallible buffers/message reservations; native thread
creation, std error formatting, libc/kernel allocation and failure cleanup remain
actual coordinator/fault/cost obligations. No whole-path panic-free or leak-free
claim is made from source or clippy.

The unchanged TCP Worker/client/completion block remains12597bytes with SHA256
`22b5864d2c0ee897cd5c12c0799181f99c1d88f2f780c64496bbf860266b47f2`.
The existing8560-byte IPv4 selector/route/nft golden passes after the shared L4
seam. That is an encoding check, not a TCP guest regression result.

## Actual light results and retained artifacts

Runner Fedora8fadc1ec213332e16cb0d915a3fb913f; admitted4CPU/1GiB/2GiB. All helper
sources/targets are private, using installed Rust1.98.1 and pinned libc0.2.189;
no workspace/C build or VM. Every revision copies exact fixture sources into a
NEW helper and NEW target, preventing stale mtimes from selecting old artifacts.

| Run/artifact | Actual result |
| --- | --- |
| `managed-network-udp-{light,target,records}-v1` | GNU compile101, helper-only E0583 from #[path] module-directory lookup; no socket/fixture test ran. |
| `...-v2` | 14pure+UDPparser1+TCPparser1 passed; GNU strict clippy101 collapsible_if; musl not run. |
| `...-v3`, source30e | 14pure+UDPparser1+TCPparser1 passed; strict GNU/musl clippy0. |
| `...-v4`, implementer light run IDr101, source9df | 14pure+UDPparser1+TCPparser1 passed; strict GNU/musl clippy0 after explicit-state correction. |
| `managed-network-udp-host-{light,target,records}-v1` | Exact extracted pure host assertion body:1test passed over8 synthetic input combinations and32 rejected bad-fact controls; strict clippy0. The helper test build also reports2 expected unused-in-non-test-lib warnings. No ignored guest caller/full host crate compilation. |
| Local | Scoped rustfmt under lockf, shell parse, Python AST parse, diff checks and policy checker passed; no local rustc/Cargo/C build. Mac free38386896KiB before formatting, above30GiB. |

The final GNU pure-test executable is frozen at
`~/managed-network-udp-proof-v4/probe-gnu-tests`, SHA256
`f7f4078d1d5c12517f4be8991c66414a45a8aee761909103d6ab9aed0d4161dc`.
It is NOT the guest artifact. Earlier source/binaries and v1–v3 raw remain intact.
`udp-light-raw-v4.tar.gz` contains104 regular-file records,142327bytes, SHA256
`3615decfba86bbcb8e862f594b5a21831c083ae972b545c5bd509e78468def80`.
All103 manifest payload sizes/SHA256 values and safe regular paths were verified
locally without overwriting an existing archive. See `udp-raw-manifest.json`.
Raw identities name source hashes even where the source commit was not yet made;
source-v4 bytes correspond to9df. No doc-test zero-test result is counted as proof.

Replay the light stages only with NEW absolute private LIGHT_ROOT, RECORDS and
CARGO_TARGET_DIR, from the actual checkout:

```sh
bash docs/research/measurements/managed-network-intercept/run-udp-light.sh
python3 docs/research/measurements/managed-network-intercept/run-udp-host-light.py
```

The two scripts require distinct NEW directories; the host script is an extracted
pure-body compile/check, explicitly not a replacement for the real host test build.

## Coordinator invocation and owed gates

Use the existing `run.sh`, eight explicit combinations of protocol udp4/udp6,
origin local/forward and mode connected/unconnected. **Coordinator only.** Retain
source/kernel/config/guest/CLI identities, actual ACKs and raw failures. Required
shared deadline, boot/memory/CPU values and fresh record directory come from the
coordinator; there are no fixture defaults for these.

```sh
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_NETWORK_EFFECTIVE_CONFIG="$HOME/managed-kernel-tproxy-5d23-v1/out/config-6.18.48-x86_64-managed-tproxy"
# Also supply SHARDS_NETWORK_CONFIG_PROVENANCE, RECORDS, BOOT_MS, CASE_MS,
# MEMORY_MIB and CPUS using their full SHARDS_NETWORK_ names, and private
# CARGO_TARGET_DIR, SHARDS_INTERCEPT_MARK/TABLE/RULE_PRIORITY.
export SHARDS_INTERCEPT_PROTOCOL=intercept-udp4
export SHARDS_INTERCEPT_ORIGIN=forward
export SHARDS_INTERCEPT_UDP_MODE=connected
bash docs/research/measurements/managed-network-intercept/run.sh
```

Supplied kernel SHA256 is
`97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae`;
config SHA256 is
`fb1a17c68b211c72fddec827313bbad0019b1165d43cc311ca2582ff91af0f8d`.
The host records those as supplied association evidence; it does not pretend
that source inputs establish the effective running configuration. Root VM envelope
remains4CPU/8GiB/20GiB; this implementer's light envelope does not replace it.

Owed: full host/guest compilation; actual8UDP guests and TCP/native-UDP regressions;
actual binary and zero-length arrivals, original destination, exact reply source,
connected-client filtering, distinct backend/relay effects and complete worker/FD/
namespace cleanup. Root selects relevant independent selector/mark/guard removals,
observes actual backend effects, byte-identical restores and resource/cost gates.
All seven supported targets and real KVM/HVF/WHP coverage remain owed for this
source. Unavailable hardware is unverified; paused Windows ARM remains untouched.

## R01–R16 account

| Rule | Applicable record and remaining gap |
| --- | --- |
| R01 | Checked counts, fallible buffers/reservations, owned spawn/joins; strict GNU/musl checks. Implicit std/libc/kernel allocation/fault behavior unproven; no panic-free claim. |
| R02 | Approved proposal and pinned Linux/RFC/man-pages/Envoy comparison above. Root actual semantic/cost evidence and product selection owed. |
| R03 | Caller IDs/deadline; OS ports/interfaces; existing documentation topology; header/ABI widths; finite binary+empty cases. No copied timing/cap/cache/default policy. |
| R04 | Explicit Exchange/stages/terminal facts; count-derived first version preserved and corrected9df. Unknown errors/joins do not pass. |
| R05 | One existing recvmsg path and shared finite preparation/L4 seam serve real native/TCP/UDP callers. Eventfd+one deadline; no cache/probes/framework. |
| R06 | Concrete UDP-only worker/state and focused functions; no generic manager. Coordinator complexity/source review remains authoritative. |
| R07 | Comments cover nonlocal reply binding, lookup exclusions, source-port ownership, empty semantics and arrival-before-validation obligations. |
| R08 | No performance comparison or4–5× achievement claim; root owns paired baselines/results. |
| R09 | Extension prepares a new experiment, not a product fix/reproduced routing bug. All new real UDP outcomes remain unrun. |
| R10 | Signed source/correction, exact commands/hashes/raw failures and focused results retained. Root before/after/mutation/restoration records owed. |
| R11 | Shared v4/v6 source and GNU/musl light checks. Whole seven-target and real VM combinations unrun; macOS arm64 only. |
| R12 | Existing socket/deadline/ns/netlink seams; no host root/new delivered binary/crypto change or OS-specific engine. |
| R13 | Peer responder only with explicit remaining mandatory routes; no architecture adoption or waived local requirement. |
| R14 | No implementer profiling/benchmarking; all allocations, kernel/parent costs, resources and4–5× target validation owed root. |
| R15 | Scoped implementation/light tests only; signed named paths/gits/trailer, no children/main merge/interruptions. Root actual guests/mutations/audits/matrix/merge. |
| R16 | Private fresh helpers/targets; old evidence/binaries untouched; no root cache writes or guard bypass. Mac above30GiB and no local compilation. |
