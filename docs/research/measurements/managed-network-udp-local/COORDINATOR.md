# Actual UDP local compatibility failure and next mechanism comparison

POLICY_READ coordinator f5cec365d2e435afae5e6f613c92d9d3759ef60f,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root r445, exact combined6e3d4cd3697aa46ac603e7e591dee30dd489733a, runs ten fresh
KVM guests on Fedora GNU amd64 with the retained experimental kernel. Actual
commands, source hashes, toolchain/kernel/config identities, inputs, output and
status are under `coordinator-6e3d/root-managed-udp-local-6e3d-v1`.

## Live red, not an accepted compatibility limit

All eight loopback/self cases fail: IPv4/IPv6 × loopback/self × connected/
unconnected. Each actual host test exits101. The guest records one `UdpSetup`
with the actual bound destination and equal held client/backend/relay namespace
inodes. Baseline and both workers are explicitly `NotStarted`. The scoped native
reply reservation returns `Fault::Io`, `AddrInUse`, errno98 at `ReserveReplyTuple`.
There are no datagram arrivals, worker completions or `UdpIntercepted` success.
The finite diagnostic driver exits0 because it observed these eight failures;
that driver code is **not** a passing UDP compatibility result.

IPv4 and IPv6 local-to-peer connected regressions each pass their actual
binary/empty datagrams, original tuples and joined workers on this same source.
The equal-namespace cases expose a limitation of the current reply reservation
strategy; they do not demonstrate an interception bypass or a product-kernel
vulnerability. No per-syscall trace was captured inside `udp_socket`, so the
record names its actual native reservation boundary rather than pretending to
prove which inner syscall emitted errno98. Baseline delivery and cleanup after
this setup failure remain unobserved. The local requirement stays OPEN.

Replay each finite case using the direct ignored-test command in README.md with
the retained explicit inputs and a new record directory. No rule or error
predicate is relaxed to make local cases green. No product remedy is committed.

## Primary alternatives inspected by the coordinator

These are design candidates, not proven solutions or authorization to ship one.
Further source and actual compatibility/cost comparisons precede selection.

| Approach | Primary evidence and constraint | Remaining proof |
| --- | --- | --- |
| Current transparent original-tuple reply socket | Linuxv6.18 [UDP port lookup](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/udp.c#L136-L197) checks namespace, local address/port and both sockets' sharing properties. Actual same-namespace reservation fails above. [TPROXY](https://docs.kernel.org/networking/tproxy.html) permits nonlocal addresses; that is not a blanket port-sharing promise. | A mechanism must preserve an arbitrary unchanged backend's native semantics while keeping reply address and port exact. |
| Add reuse flags | The same pinned lookup checks both sockets; an option applied only to a new relay does not establish an arbitrary backend's cooperation. Reuse-port also participates in receive selection. | No live claim that this fixes the red. Changing the backend or closing it would invalidate the independent endpoint witness, so neither is used. |
| Follow Envoy's source-IP transparent UDP model | Pinned [Envoyv1.36.0 documentation](https://github.com/envoyproxy/envoy/blob/v1.36.0/docs/root/configuration/listeners/udp_filters/udp_proxy.rst#L10-L25) explicitly preserves source IP without upstream source-port preservation. Its [send path](https://github.com/envoyproxy/envoy/blob/v1.36.0/source/extensions/filters/udp/udp_proxy/udp_proxy_filter.cc#L478-L510) uses a selected local IP with a separately allocated socket. | A different transparency contract is not native port compatibility; this alone is insufficient for Shards' exact-tuple requirement. |
| Kernel address translation | Pinned [nft NAT validation](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_nat.c#L127-L151) supports source NAT at postrouting/local-input and destination NAT at prerouting/local-output. Both families are represented. The supplied experimental config already enables NFT_NAT/NF_NAT, but runtime behavior is untested. | Conntrack interaction, original-destination recovery per datagram, collisions, connected clients, loopback, revocation and attribution; a return-only rewrite cannot be assumed to work. |
| Cgroup socket-address hooks | Pinned Linuxv6.18 [connect4 selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/connect4_prog.c#L140-L188) and [sendmsg4 selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/sendmsg4_prog.c#L19-L45) demonstrate checked destination rewriting at distinct call paths. This is the alternative already named in the managed plan. | Both families, connected/unconnected/sendmmsg/recvmsg/getpeername presentation, received FDs, identity/lease state, unsupported-path denial and actual kernel/user costs; connect alone is incomplete. |
| Explicit application UDP proxy | [RFC9298 §3.1](https://www.rfc-editor.org/rfc/rfc9298.html#section-3.1) supplies explicit request/socket ownership and source-tuple validation for unconnected proxy sockets. | Useful broker contract, not evidence that arbitrary native UDP callers already obey it or that transparent coverage is complete. |

Kernel TPROXY [IPv4](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/netfilter/nf_tproxy_ipv4.c#L111-L127)
and [IPv6](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/netfilter/nf_tproxy_ipv6.c#L112-L127)
also distinguish connected sockets during established/listener lookup. A different
reply strategy must preserve this packet-selection behavior, not merely pass bind.
No new cache, polling, raw-packet generator, NAT installer or BPF abstraction is
introduced by this comparison. The direct Envoy rendered-documentation URL and
one BPF documentation URL were unavailable through the web reader; the pinned
source reads above succeeded. Retrieval failure is not a runtime result.

## Policy and remaining gates

R01/R04/R06/R07/R13: precise native setup cause/identity and explicit NotStarted
states; no invented success, baseline, join or cleanup result. R02/R03/R05/R12:
primary alternatives and compatibility differences recorded before selection;
no workaround, new runtime literal/cache/manager/platform implementation or
host-root/security/crypto change. R09/R10: all eight actual failure effects and
two passing peer controls, raw/source identities and exact transfer SHA retained.
R08/R14: no performance or allocation-free claim; parent/kernel accounting and
paired costs remain root obligations. R11: only GNU amd64 KVM; other platforms,
seven-target compilation of this extension and actual supported hypervisors are
owed, Windows ARM paused. R15/R16: root actual campaign, implementer light/source
only, private paths and immutable original evidence preserved. Main adoption,
full forwarding, hard budgets, other UDP modes/routes and meaningful remedy
mutations/restoration remain OPEN.
