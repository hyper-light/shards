# Native managed-network probes (P4; G04–G06)

This is an experimental observation harness, not the implemented mediation layer
or a passing real-VM result. It adds a mode to the existing static testguest binary;
there is no new installed binary, shell supervisor or product route change.
The coordinator runs all real VMs, interception comparisons, mutation/chaos cases
and cost measurements. The implementer runs parser/compile/light checks only.

Policy read: revision `6bf5b15da904931b984ecd25662a8b8c571268bf`, role implementer,
R01–R16 SHA-256 `0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
Owned paths: this directory, `crates/testguest/src/managed_network.rs`, and its
small dispatch in `crates/testguest/src/main.rs`. The capture lane owns workload.rs.

## Contract

```
testguest managed-network capabilities [EXACT_BUILT_CONFIG]
testguest managed-network serve tcp|udp BIND REPLY_HEX REQUEST_HEX DEADLINE_MS native|transparent
testguest managed-network dial tcp|udp|udp-connected DEST REQUEST_HEX REPLY_HEX DEADLINE_MS native
```

Addresses are numeric IPv4 `IP:PORT` or IPv6 `[IP]:PORT`. Bind port zero asks the
kernel for a free port; read the flushed `Listening` observation before launching
the client. A destination names the address/port actually granted by the experiment.
All payload bytes are explicit hex, including empty strings, NUL and invalid UTF8.
TCP writes the specified bytes, closes its write half, and requires exactly the
expected reply plus peer EOF. The server requires the client's write half to end
before replying, so it can prove a half-close preserved the receive direction.
The observed per-socket local/peer tuples come from the kernel, never payload text.

UDP performs one datagram exchange per invocation; `udp-connected` uses connect
plus send, while `udp` uses send_to. `recvmsg(MSG_TRUNC)` rejects an oversized or
short datagram and ancillary truncation; empty payload is still one datagram.
IP_RECVORIGDSTADDR/IPV6_RECVORIGDSTADDR report the received original destination.
The client checks the actual reply source equals its requested destination.
A transparent wildcard UDP listener replying from its proxy port may therefore
fail that check: this is a result to retain, not a reason to waive source semantics.
This first slice does not implement a transparent UDP reply socket or relay.

Every socket operation uses one absolute, caller-provided deadline across the
whole case. Zero expires before I/O; an overflowing Instant deadline is refused.
There is no picked test timeout. Root chooses its experiment deadline from its
own test/liveness budget and retains it with the run. Payload allocation is exactly
the supplied expected size, fallible, and never a network-advertised preallocation.
Short TCP writes and EINTR/WouldBlock retry within that same deadline.

Observed states are `Listening → Connected → PeerClosed → Received → Sent →
WriteClosed → Matched` on the TCP server and `Connected → Sent → WriteClosed →
PeerClosed → Received → Matched` on the client. UDP is `Listening → Received →
Sent → Matched` or `Sent → Received → Matched`. Any error is a terminal nonzero
exit with error kind/errno. No observation asserts a Shards grant or exact tool
identity. `Matched` proves only the endpoint's specified socket/payload behavior.

`capabilities` reports kernel release, network namespace, transparent bind results
and named symbols from the exact uncompressed built config if provided. Successful
IP_TRANSPARENT does not prove an nft hook exists. Missing config remains explicitly
unverified. The current base configs disable IKCONFIG; retain the actual booted
kernel asset's hash and its workflow-produced config alongside the observation.
Do not label a checked-in input config as the effective built config.
Expected symbols and missing/invalid values are printed independently.

Transparent listeners belong to the trusted experimental relay namespace/process.
The ordinary actor's refusal to set IP_TRANSPARENT is useful negative evidence;
do not add a network capability to an actor merely to make this probe pass.

## Research and alternatives

The existing [relay review](../../managed-runtime-buffered-relays.md) and
[control inventory](../../managed-runtime-design.md) identify the current guest
Switch/Gate seam and external host relay. This slice changes neither.

| Candidate | Primary evidence | What this harness must establish |
| --- | --- | --- |
| nft TPROXY plus transparent socket and policy routing | [Linux TPROXY contract](https://docs.kernel.org/networking/tproxy.html); [Linux v6.18 nft_tproxy.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/net/netfilter/nft_tproxy.c), validate/eval functions; [kernel TCP selftest](https://raw.githubusercontent.com/torvalds/linux/v6.18/tools/testing/selftests/net/netfilter/nft_tproxy_tcp.sh) | Original tuples, loopback/self traversal, TCP/UDP and both families. The implementation restricts TPROXY to PREROUTING and TCP/UDP, so switch-only installation is insufficient evidence. The kernel selftest distinguishes proxy from backend responses; use the same decisive-observer principle. |
| cgroup socket-address redirect plus owned relay | [v6.18 connect4 selftest](https://raw.githubusercontent.com/torvalds/linux/v6.18/tools/testing/selftests/bpf/progs/connect4_prog.c) | Its source rewrites destination fields at connect. That alone does not establish coverage of unconnected UDP/sendmsg, listeners, reply tuples, received FDs or every namespace route. Those remain distinct required cases. |
| REDIRECT/DNAT to a proxy | [Linux TPROXY contract](https://docs.kernel.org/networking/tproxy.html), redirection section | NAT changes the destination; original-destination recovery has limits. Compare actual native tuple semantics rather than claiming transparent equivalence. |
| Environment proxy or application broker | Existing review and [HTTP CONNECT RFC9110 §9.3.6](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.3.6) | A cooperative application path is not mandatory interception of an arbitrary native process's loopback or internal socket. It may be a distinct explicit semantic adapter. |

`experimental-tproxy.config` requests only NFT_SOCKET and NFT_TPROXY over the
existing Shards common config. [Linux v6.18 Netfilter Kconfig](https://raw.githubusercontent.com/torvalds/linux/v6.18/net/netfilter/Kconfig)
selects the IPv4 and enabled-IPv6 socket/tproxy and defragmentation dependencies.
The existing inputs already enable nftables, family tables, both policy-routing
families and cgroup BPF. Root must inspect olddefconfig's **effective** output and
runtime behavior after any experimental build. No production pin/config is changed.
Kconfig availability, byte/cost impact and failure behavior are still unmeasured.

## Coordinator run matrix

Build the ordinary testguest through the repository's guest target/toolchain; use
that same artifact in a root agent.json or harness.json with literal options.
HARNESS/AGENT supplies it; NETWORK/CONNECT/EXPOSE grant the tested route explicitly.
Do not install it through RUN/VOLUME. A plain workload may use the image's copied
testguest as existing tests do. Use fresh test-only paths and homes; no credentials.

For a single native TCP exchange, substitute actual addresses, a caller-derived
deadline and the server's allocated port:

```
testguest managed-network serve tcp 127.0.0.1:0 7265706c7900ff 726571756573740080 DEADLINE_MS native
testguest managed-network dial tcp 127.0.0.1:ALLOCATED_PORT 726571756573740080 7265706c7900ff DEADLINE_MS native
```

The server is in the same domain for loopback/self cases and a distinct granted
domain for inter-domain cases. Use the actual own-link address for self-address
tests, `[::1]`/actual IPv6 for IPv6, and wildcard server binds separately. The byte
strings above are fixture data (`request`/`reply` plus binary bytes), not runtime
limits. Repeat UDP with both client modes, empty payloads, independent request and
reply sizes, and a deliberate extra/short datagram as negative cases.

| Case | Required effect evidence |
| --- | --- |
| Same-domain 127.0.0.1 / ::1, own address, wildcard listener | Client and receiver agree exact bytes and actual tuples. Observe trusted relay separately; no relay event alone proves where bytes went. |
| Inter-domain IPv4/IPv6, peer/engine and external route | Retain all endpoint tuples, received bytes, grant/interception config and actual relay/backend observations. |
| TCP half-close, no newline, NUL/invalid UTF8 | Server receives complete request+EOF and client still receives complete binary reply+EOF. |
| UDP connected/unconnected, zero-length, truncation | Preserve datagram length/boundaries and original destination; oversize/short/truncated ancillary fails explicitly. Reply source must match requested destination. |
| Remove only tested interception guard | Give trusted relay and backend different expected reply bytes. The actual backend must receive the formerly intercepted bytes and the client must detect the changed reply. Alert disappearance alone is insufficient. |
| Disable required kernel feature / kill relay / revoke route | Managed launch or flow refuses; no direct backend receives bytes. This probe itself does not implement fail-closed policy. |
| Exemption/mark/received socket/Unix/FD bypass | Distinct tests remain owed; this first IP endpoint probe does not claim coverage. |

Root also owes multi-destination UDP/sendmsg metadata, QUIC, PMTU/fragmentation,
RST/TIME_WAIT, nonblocking boundary races, fairness/backpressure, induced kernel
charges and allocation/fault/cost distributions. Compare alternatives with equal
grants/security, record misses and publish actual 4–5× target ratios only if measured.
No topology, nft policy, relay or mechanism choice is silently installed by this
harness. Existing anti-spoofing/grants/conntrack remain independent requirements.

## Light validation and rule account

From a Linux checkout, `sh docs/research/measurements/managed-network/light-check.sh`
compiles the exact module and runs parser/deadline/config tests in a temporary
libc-only crate using the repository's locked libc version and clippy test policy.
It does not open a socket, run a VM, build a kernel or replace the full testguest
compile/target gate. Temporary generated source/build files are removed on exit.
Format: `rustfmt --edition 2024 --check crates/testguest/src/managed_network.rs crates/testguest/src/main.rs`.
Run `python3 scripts/check-development-policy` before readiness.

| Rule | Scoped account / owed coordinator evidence |
| --- | --- |
| R01 | Fallible allocations, checked deadline and slice access; existing deny lints retained. Full target lints and coordinator panic-path audit remain required. |
| R02 | Established kernel TPROXY/selftest, cgroup-connect and NAT/proxy alternatives above. Root live comparison still owed before product mechanism selection. |
| R03 | No timeout/port/buffer tuning value added. Deadline/payload are supplied, port zero is OS allocation, backlog one is the one-connection case, control space derives from sockaddr/cmsghdr ABI. Hex radix/digit arithmetic derives from encoding. |
| R04 | Probe roles and observable protocol states are enums; legal order is documented above and errors terminate. No product lifecycle or implicit policy fallback introduced. |
| R05 | One native probe module with concrete server/client consumers and small main dispatch; no new manager/trait framework. |
| R06 | Focused parser/socket/transfer/report functions; root reviews the source and integration. |
| R07 | Comments cover safety, protocol/ABI and scope; references/method are here. |
| R08 | Comparative result/4–5× claim not measured. Root owns baselines and measurements. |
| R09 | No product bug fix or claim of live reproduction. Root runs the actual gap examples before selecting product changes. |
| R10 | Exact commands, fixture/tuple/payload contract and mutation matrix retained here; root real-VM logs/traces still owed. |
| R11 | One Linux guest implementation for both architectures; host dispatch stays Linux-only. Seven-target gate and KVM/HVF/WHP runs owed; paused Windows ARM VM untouched. |
| R12 | Extends carried testguest only; no installed helper, host root or TLS weakening. |
| R13 | First endpoint/capability slice only; no product hook, route or speculative relay. |
| R14 | Bounded expected payload and ancillary buffers; allocations/parent/kernel costs require root profiling before performance claims. |
| R15 | Implementer runs only targeted/light checks; root owns all audits/heavy E2E/failure/mutations/benchmarks and merge. |
| R16 | Temporary owned check caches only; preserve evidence/source/data. Keep Mac >=30GiB and respect kiln admission. |
