# Socket-address hook: finite TEST-ONLY comparison

Implementer branch starts at `c7e87dc506abfe73a7839a32f8ecc514993ab63c`.
Policy read in full: canonical g121-main `f5cec365d2e435afae5e6f613c92d9d3759ef60f`,
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root owns source review, all actual guests/BPF loads, mutations, costs and selection.

This is destination-rewrite comparison source, not product BPF adoption or a fix
for the actual UDP-local `ReserveReplyTuple/AddrInUse/98` results recorded in
[the coordinator record](../managed-network-udp-local/COORDINATOR.md). No actor
cooperation, backend modification, NAT, raw packets, reuseport, transferred backend
FD, presentation hook, legacy attach fallback or installed helper is introduced.

## Finite source contract

One fresh network namespace contains a native IPv4 loopback backend and separate
native responder on OS-assigned ports. PID1/control/responders stay outside the
fresh child cgroup. A dedicated new mount namespace holds a private cgroup2 mount
and one fresh leaf; current namespaces are never administered. Both fresh exec
clients enter the held network namespace and write their own membership to the
already-opened leaf `cgroup.procs` before exec and before socket creation.

The baseline runs the real Binary/Empty script against the backend. Then one
unpinned link attaches CONNECT4 for connected UDP or UDP4_SENDMSG for unconnected
UDP. Five selectors (AF_INET, SOCK_DGRAM, UDP, exact destination IP AND port)
precede two destination writes. Unmatched operations are allowed by this isolated
comparison, which is **not** a default-deny policy or bypass-closure proof. Existing
`shards_devcgroup::Insn::to_bytes` is reused unchanged; device-specific loaders
remain private and unchanged. The private sock-address ABI adapter serves only
these two concrete hook variants. No maps or dynamic flow history exist.

The client records actual getpeername and actual datagram length/flags/copied bytes/
peer/original-destination metadata BEFORE validation. A mismatch accumulates while
both Binary/Empty exchanges continue where I/O permits; its final native verdict
is ALWAYS failed. It never calls one exchange ScriptComplete. IPv4 endpoint AND
port remain strict. Backend/responder use the existing UDP worker algorithm
byte-for-byte apart from necessary sibling visibility. Responder replies carry
RELAY bytes; baseline replies carry BACKEND bytes; empty replies stay empty.
These are responders, **not** a forwarding relay.

The original work Deadline bounds parent pidfd readiness and worker I/O. A child
failure cancels only that owned child and reaps it; workers use their existing
eventfd cancellation and join. The link/cgroup/namespace handles remain held
through reap and joins; no reader is detached. Reap failure emits Unknown and
retains the scope pending the coordinator's enclosing VM lifetime deadline. A
finite work deadline does not prove cleanup completed by that instant. Only after
those completions does explicit LINK_DETACH run, leaf FDs close, the kernel removes
the leaf, and the private mount is unmounted/removed. Every normal cleanup fact is
recorded; an error remains nonzero. Child reports are copied from finite private
files after reap, avoiding detached pipe readers and cross-process record mixing.

## Primary sources and alternatives

All kernel references are pinned to Linux **v6.18**, distinct from the coordinator's
experimental 6.18.48 build identity (which must still be supplied and probed).

- [bpf.h](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/bpf.h):
  `bpf_sock_addr` field offsets/read/write widths/network order, program type18,
  CONNECT4 attach10, UDP4_SENDMSG attach14, PROG_LOAD5, LINK_CREATE28,
  LINK_DETACH34, instruction opcodes and attribute prefixes. The 16-byte object
  name, eight-byte instruction, four-byte words and 16-bit branch offsets are ABI
  widths, not selected resource caps. Encoding capacity derives fallibly from the
  actual five comparisons, two writes and allow/exit sequence. No verifier log
  buffer is guessed: `NotRequested` is explicit; load failure retains errno.
- [connect4 selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/connect4_prog.c)
  and [sendmsg4 selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/sendmsg4_prog.c):
  real writable destination seams. Unrelated selftest socket settings/source binds
  and default ports are not copied.
- [udp.c](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/udp.c):
  UDP pre-connect versus `!connected` SENDMSG paths are different real callers;
  receive presentation has a separate RECVMSG hook. Ancillary original destination
  is not recovered from our configured destination.
- [af_inet.c](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/af_inet.c):
  native connect and getpeername use stored endpoint state; GETPEERNAME is a
  separate optional hook that this first slice does not install.
- [cgroup.c](https://github.com/torvalds/linux/blob/v6.18/kernel/bpf/cgroup.c):
  socket-address hooks use the socket's cgroup; precreated/transferred socket
  authority is not solved by moving a caller. Unpinned cgroup link release detaches.
- [nft_nat.c](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_nat.c):
  NAT-chain/hook restrictions; return-only NAT and conntrack/original-destination
  collision behavior remain unproven. No NAT fallback is implemented.
- [Envoy v1.36 UDP proxy](https://github.com/envoyproxy/envoy/blob/v1.36.0/docs/root/configuration/listeners/udp_filters/udp_proxy.rst)
  preserves original IP in its transparent mode but explicitly not upstream source
  port. That established alternative cannot justify relaxing this contract.
- [RFC9298 section3.1](https://www.rfc-editor.org/rfc/rfc9298.html#section-3.1):
  UDP connect is not delivery evidence; connected filtering and explicit
  unconnected peer validation matter. The exact source port remains required.

## Light results and replay

Private Fedora `8fadc1ec213332e16cb0d915a3fb913f`, admitted4CPU/1GiB/2GiB,
`rustc1.98.1`, new `~/managed-network-sock-addr-proof-v4` source/target.
`light.py` copies actual source bytes into NEW directories without retaining
source mtimes, records source hashes and commands, and executes only pure units
and source compilation. Root's clones/targets were untouched. No local compilation.

```sh
python3 docs/research/measurements/managed-network-sock-addr/light.py \
  "$HOME/managed-network-sock-addr-proof-NEW"
```

Actual r123: GNU pure units0, musl pure units0, GNU clippy `-D warnings`0,
musl clippy `-D warnings`0. Earlier r120/r121 retain helper module/include path
and source type-inference reds; r122 had both unit suites0 then clippy101 for
private visibility/collapsible-if, corrected in r123. These are compiler/fixture
results, never actual routing or BPF verifier results. The subsequent final source-linked r128 has both pure unit suites0 and GNU/musl
clippy0 with the workspace panic-lint subset, unsafe-op and undocumented-unsafe
checks explicitly denied (see raw commands). Its only guest-source change is one
adjacent SAFETY comment after actual r127 lint101. Full workspace lints, actual
host common implementation, seven-target matrix and all runtime behavior remain
unrun in this lane.

Host extraction r124 actually failed on a missing assertion separator (a Linux ABI
comment swallowed the comma); signed a3296a665 corrects only that syntax. Actual
r125 then failed E0308 on integer0 versus common::Run status Option<i32>; signed
f35e1d48f corrects only Some(0). Actual r126 host extraction compile0 and exactly
one pure record-parser test0. `light-host.py` copies the actual host caller but
uses explicitly inert, type-matching common stubs; it does not compile or run real
VM helpers and cannot replace coordinator full host compilation.

A source concern about the new mount cleanup thread using setns with shared
CLONE_FS is **unverified**. Root requires actual runtime reproduction before any
remedy under R09. No CLONE_FS change or draft exists in this checkpoint. Root owns
that reproduction; source suspicion is not a confirmed failure.

Raw compiler logs/commands/source snapshots/hashes and test-binary identities are
retained in `light/`; no executable binary bytes are in the archive. Initial failed
helper setups and before-correction source snapshots are preserved. `pack-light.py`
replays the safe artifact packaging. No source/evidence/compiler cache was deleted.

Coordinator invocation, with caller-supplied absolute experimental kernel/config
and prebuilt source-matching CLI/helpers/testguest (never production pin fallback):

```sh
SHARDS_SOCK_ADDR_MODE=connected \
SHARDS_KERNEL=/absolute/experimental/vmlinux \
SHARDS_NETWORK_EFFECTIVE_CONFIG=/absolute/experimental/config \
SHARDS_NETWORK_CONFIG_PROVENANCE='exact source/build/config association evidence' \
SHARDS_NETWORK_RECORDS=/absolute/private/records \
SHARDS_NETWORK_BOOT_MS="$CALLER_BOOT_MS" SHARDS_NETWORK_CASE_MS="$CALLER_CASE_MS" \
SHARDS_NETWORK_MEMORY_MIB="$CALLER_MEMORY_MIB" SHARDS_NETWORK_CPUS="$CALLER_CPUS" \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  direct_guest_socket_address_udp4_requires_original_native_tuple -- --ignored --exact --nocapture
```

Replay separately with `SHARDS_SOCK_ADDR_MODE=unconnected`. The host fixture retains
raw identities/output/status BEFORE strict assertions. A failed original tuple is
an experimental RED, never accepted as a new passing contract. Root must inspect
both clients' datagrams and worker counts even when an earlier host assertion
reports failure. Current source does not predict or claim observed runtime status.

## Rule-by-rule evidence and owed work

|Rule|This scoped checkpoint; remaining proof|
|---|---|
|R01|Scoped GNU/musl panic-lint subset and unsafe-comment checks0 (not full workspace/fault proof). New encoding reserves derived capacity, checked arithmetic/widths/slices, no explicit panic outside unit assertions. Standard Command/path/file/report and thread runtime allocations are not fault-injected or proven panic-free. Full workspace lints/fault behavior owed.|
|R02|Pinned primary mechanisms/alternatives above; actual UDP-local red linked. New comparison has no live outcome yet; root selects after real evidence.|
|R03|UAPI/protocol field widths and finite Binary/Empty semantic script; actual OS ports; caller deadline. No arbitrary replacement cap/default timeout.|
|R04|Controller/client roles, typed stages, Exact/Mismatch, Running/Reaped, mount lifecycle, shared worker states. Actual error/reap/join/detach paths owed.|
|R05|One existing UDP worker implementation, actual readiness/pidfd/eventfd/joins; no periodic probes/cache/maps/generic loader. Current work versus outer VM deadline distinction explicit.|
|R06|Private fixed ABI adapter, two real hook callers, finite straight-line emitter; coordinator source review pending.|
|R07|Comments identify unsafe/pre-exec/ABI/ownership obligations; citations above. Actual r127 missing-adjacent-comment red retained, r128 corrected green.|
|R08|No speed/resource claim;4–5x target not measured/achieved.|
|R09|Prior local original-tuple reservation failure is actual coordinator evidence. This alternative has no live reproduction/remedy claim.|
|R10|Runnable source/light driver and exact owed guest invocation; raw compiler packaging separate; coordinator runtime/mutation logs owed.|
|R11|Light Linux GNU/musl only; all seven-target/full guest matrix owed. IPv4-only first mechanism comparison.|
|R12|Existing emitter and concrete native UDP machinery reused; no installed helper, hostroot or crypto changes. Existing DEVICE loaders stay unchanged.|
|R13|Test-only scoped destination comparison; no general policy/relay/flow manager.|
|R14|All allocations/faults/CPU/kernel charges and cost comparisons root-owned and unrun.|
|R15|Implementer light-only; no guest/BPF/socket/namespace execution, mutation, audit/profile/heavy campaign, children or main merge. Source review before root execution.|
|R16|Mac actual35.7GiB before edits; lockf formatting; no local compiler. New private Linux source/target, no active cache/source/evidence deletion.|

Still unverified: actual kernel support/verifier/link semantics, connected filtering,
getpeername/recvmsg presentation/ancillary identity, inherited/transferred sockets,
IPv6/multi-destination/own-address/wildcard/fragment/PMTU/QUIC, forwarding relay,
fail-closed grants/revocation/death, controller/worker/child failures, cgroup placement
and cleanup/resource return, original endpoint collision solutions, cost and matrix.
No authority/compatibility requirement is waived by this experiment.
