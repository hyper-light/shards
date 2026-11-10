# Native IPv6 TCP interception fixture

Status: **fixture prepared; actual IPv6 interception/absence behavior unrun by this
lane**. Signed source `15a54a3aab2a82197be6afc306e3004377865bbe`, pushed on
`p/managed-network-tcp6`, new worktree from preserved
`91b6e0cb8e5168353100ff1c479d1067a0ea0637`. Root approved the concrete native
family boundary before edits, including per-address NODAD for exactly four fixed
documentation addresses in NEW guest namespaces. No product mechanism adoption,
kernel/config/pin, security/grant, installed binary or host namespace change.

POLICY_READ implementer: full current g121-main AGENTS/R01–R16/evidence/CLAUDE/
managed plan/current handoff, revision `f5cec365d2e435afae5e6f613c92d9d3759ef60f`,
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Own intercept.rs/wire.rs, adjacent parent parser/usage and pure parser control,
existing host ignored fixture/assertions, experiment drivers/docs only. Root owns
all audits/profiling/benchmarks/heavy guests/chaos/sweeps, actual mutations,
restoration, resource/cost/matrix validation, selection and main merges. No children.
Root r286's separate clone/cache were left untouched; its bounded IPv4 campaign
was not rerun or audited by this lane.

## Explicit mode, same native owners

```text
managed-network intercept-tcp6 ORIGIN DEADLINE_MS MARK TABLE RULE_PRIORITY
```

ORIGIN retains exactly local, forward, local-loopback, local-self and
forward-relay-absent. The explicit command selects Family::Ipv6; intercept-tcp4
selects Family::Ipv4. Both use the same run, native SocketAddr/transparent socket,
client, TCP half-close/exact binary bytes, Worker/Completion, original deadline,
eventfd cancellation and bounded absence/refusal verdict. No second engine,
resolver, route manager, generic framework, cache or readiness polling is added.

| IPv6 role | Actual fixture address/namespace |
| --- | --- |
| Router toward backend / peer backend | 2001:db8:1::1 and2001:db8:1::2 on the first /64 veth link |
| Router toward client / peer client | 2001:db8:2::1 and2001:db8:2::2 on the second /64 veth link |
| local | Client in NEW router; backend listener in NEW peer backend |
| forward | Client in NEW peer client; backend listener in NEW peer backend |
| local-loopback | Client/backend listener/transparent responder in NEW router; actual target::1 |
| local-self | Same NEW router roles; actual target2001:db8:1::1 |
| forward-relay-absent | Same forward roles and working baseline; sole transparent listener closes before fresh call; relay worker never starts |

Exactly the existing three namespaces and two links are used. All administration
occurs only after unshare/setns into a NEW held guest object. The IPv6 router's
forwarding sysctl is written there; no global DAD or host setting changes.
Ports are OS-assigned and interfaces are resolved from real objects. Target JSON
adds family derived from the actual bound destination. Existing actual namespace
inodes, baseline BACKEND, distinct RELAY reply, original tuple, independent
accept/receipt counts, joined completions and typed errors remain authoritative
observations. A declared family/config value is not an actual interception pass.

The host has separate ignored tcp4/tcp6 entrypoints sharing one assertion body
with two real family callers. It parses actual destinations as SocketAddr and
compares typed IPs, roles, bytes, tuples and all bounded-verdict branches. The
native engine and the entire worker/client/completion/absence source block remain
**byte-identical91b6**, with exact slice boundaries/hashes in
[tcp6-sources.json](tcp6-sources.json). In particular, no timeout/error is broadly
accepted and no behavior after the observation window is promised.

## Pinned derivation and alternatives

Downloaded identities of18 primary files are in
[tcp6-primary-sources.json](tcp6-primary-sources.json); full retained bytes are
`/Users/adalundhe/g121-managed-network-tcp6-primary`.

- [Linuxv6.18 selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/net/netfilter/nft_tproxy_tcp.sh#L52-L80)
  uses IPv6 /64 links with explicit nodad addresses, forwarding, and both local
  and forward scenarios. Its [IPv6 rules](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/net/netfilter/nft_tproxy_tcp.sh#L154-L205)
  use OUTPUT route marking, fwmark policy and a selected host local route before
  PREROUTING TPROXY. We reuse these semantics with native acknowledged messages,
  owned sockets and events; no ip/nft/socat/shell actor or copied timeout/port.
- [RFC3849](https://www.rfc-editor.org/rfc/rfc3849.txt) supplies2001:db8::/32 for
  documentation identities; [RFC4291](https://www.rfc-editor.org/rfc/rfc4291.txt)
  gives IPv6's128-bit addressing, loopback and unicast layout. /64 is the explicit
  link topology used by the pinned selftest, not a learned resource limit; /128
  selects the actual single listener address. [RFC5952](https://www.rfc-editor.org/rfc/rfc5952.txt)
  informs presentation; host checks typed IP values instead of requiring a spelling.
- [IPv6 UAPI header](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/ipv6.h#L111-L130),
  [in6_addr](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/in6.h#L30-L53)
  and [RFC8200](https://www.rfc-editor.org/rfc/rfc8200.txt) establish a16-byte
  destination at fixed-header offset24. The selector retains META_L4PROTO and
  transport-relative TCP destination-port offset2, not an assumed fixed complete
  IP header length. Register/attribute lengths and conversions remain checked.
- [nft_tproxy IPv6 evaluator](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_tproxy.c#L77-L141)
  uses kernel transport offset/protocol and transparent socket lookup; missing
  listener BREAK semantics retain the same **separate following exact-selector
  DROP rule**. Only the nft family/family attribute changes for IPv6, with a
  specific NFPROTO_IPV6 table; existing IPv4 message bytes are preserved.
- [nft_chain_route.c](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_chain_route.c#L59-L103)
  implements IPv6 OUTPUT route reevaluation on mark change under NF_TABLES_IPV6.
  No nonexistent separate route-chain option is introduced. Actual netlink ACK
  refusal/capability failure remains a typed error, not a config workaround.
- [if_addr.h](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/if_addr.h#L40-L49)
  defines IFA_F_NODAD=0x02; [addrconf.c](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/addrconf.c#L4655-L4697)
  parses it. The builder explicitly rejects any IPv6 assignment outside the four
  approved documentation IDs before setting this flag. This avoids DAD readiness
  polling only in the closed synthetic topology; it is no production DAD policy.
- [fib6_rules.c](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/fib6_rules.c#L555-L585),
  [fib_rules.h](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/fib_rules.h)
  and [rtnetlink.h](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/rtnetlink.h)
  define AF_INET6 rules/fields and defaults0/local,0x7ffe/main. The same checked
  caller-selected mark/table/priority authority is retained.
  [IPv6 route parsing](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/route.c#L4839-L4875)
  and [iproute2v6.18.0](https://github.com/iproute2/iproute2/blob/v6.18.0/ip/iproute.c#L1513-L1519)
  justify IPv6 local-route scopeUNIVERSE while IPv4 keeps HOST. Address/gateway
  attributes have their actual4/16-byte widths; mixed gateway families refuse.

A duplicate IPv6 socket/worker engine would duplicate ownership and state already
served by SocketAddr; a mixed inet table would change the existing IPv4 encoding
without need. Per-address static nodad is the approved selftest setup; global DAD
changes or a new address-readiness manager/poll would widen this fixture. No
additional architecture, authority or performance choice is adopted. Message
capacity still derives from actual arrays and every append remains fallible.

## Actual light evidence and IPv4 byte preservation

Private Fedora r92/v1, exact4CPU/8GiB/20GiB, Rust1.98.1: **11/11 pure interception
units +1/1 exact parent parser unit**, strict amd64 GNU clippy0 and musl clippy0.
Tests decode IPv6 address/fib/route/nft widths/family/prefix/selector/DROP ordering,
reject family/prefix/assignment mismatches, retain bounded-state cases and accept
only the two explicit protocol entries. No socket, namespace or guest was run.
Host changes are format/syntax checked, **not compiled/executed** by this lane.

Before family edits, r90 captured8560 deterministic bytes from the real existing
pure selector, selected local-route and full nft Message kind/flags/body encoders
for the five IPv4 cases at91b6. Test-only length framing is documented in the
retained [capture patch](ipv4-before-capture.patch), which applies to91b6; it
introduces no runtime change. The old runtime prefix was checked identical91b6.
Final unit asserts exact equality against [ipv4-wire-before.hex](ipv4-wire-before.hex),
SHA256 `637e2c05b8256410af49939f40b6a4739f8956ee8728719948e8336004fd56ba`.
This is a test regression artifact, never a runtime number/cap. It is not a packet
trace or a substitute for restored IPv4 guest runs. V4 address/fib ABI fields
also retain independent decoded assertions.

Private clone `~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-network-tcp6-605c406ce9`,
this lane's own `~/managed-network-relay-absent-target`. Package clean/source touch
force freshness, actual compiler logs/source hashes distinguish the uncommitted
patch from base91b6. No timing instrumentation. Frozen final GNU unit SHA256
`8d170e3e1a389f898d74a3081a4e2407d4f3c0a30ea55f4f46b9478a74db2c0e`
at `~/managed-network-tcp6-proof/v1/shards_testguest-975e61748c61ff31`; before binary
SHA256 `a0a5889a2efbd6763be059ccfc2e92a9b51165f65356fdb78f75d9800ca7b1df`
is retained in proof/ipv4-before. Source capture remains
`/Users/adalundhe/g121-managed-network-tcp6-baseline-source/wire-with-capture.rs`.

[tcp6-light.tar.gz](tcp6-light.tar.gz), SHA256
`c8283b9cb72723601eba1c5417e0407c06792559a6c640118c83c93e6e36e60f`, contains23
verified raw command/log/exit/compiler/source/binary/baseline records and internal
manifest. Expanded copy `/Users/adalundhe/g121-managed-network-tcp6-light`.
The [light driver](run-light.sh) used this private target,
RECORDS=`$HOME/managed-network-tcp6-light-v1`, musl CC/AR=this checkout's scripts/
zig-cc and zig-ar, ZIG_TARGET=x86_64-linux-musl. Each exact command/exit is retained.
[Local checks](tcp6-local-checks.txt) format/shell/policy/whitespace0, no local
compiler/Cargo/VM build, actual free39209476KiB>30GiB. No cleanup/deletion/admission
bypass, root/other-lane cache write or active lane job.

## Exact owed coordinator entry and limits

Use existing reviewed absolute experimental SHARDS_KERNEL/effective config/
provenance and explicit SHARDS_NETWORK_BOOT_MS/CASE_MS/MEMORY_MIB/CPUS plus mark/
table/priority, private target and unchanged4CPU/8GiB/20GiB admission:

```sh
export SHARDS_INTERCEPT_PROTOCOL=intercept-tcp6
export SHARDS_INTERCEPT_ORIGIN=local
export SHARDS_NETWORK_RECORDS="$HOME/managed-intercept-reviewed-tcp6-local"
bash docs/research/measurements/managed-network-intercept/run.sh
```

Repeat separately with fresh record directories for forward/local-loopback/
local-self/forward-relay-absent. Exact ignored filter is
`experimental_tcp6_interception_has_distinct_backend_and_relay_observations`.
Old driver calls keep tcp4; any other explicit protocol refuses. Zero-test/no
actual observation cannot pass. Kernel/config identities remain in
[RELAY-ABSENT.md](RELAY-ABSENT.md#owed-coordinator-execution). Inputs remain supplied
evidence, not inferred effective built config association.

Root owes host compile/parser, actual capability/ACK/error, family/namespace/
original tuple/bytes/half-close/count/joins and bounded-window behavior for all
five IPv6 cases, independent relevant guard removals showing actual backend
effects, byte-identical restoration, IPv4 regression, ownership/resource/cost/
seven-target/hardware gates. All are **unverified here**, not passing skips.
Extension headers, fragments, link-local/scope IDs, mapped IPv4, UDP/QUIC, Unix/
FD transfer, real forwarding relay and full route mediation remain unverified.
The existing native UDP primitives are untouched for a later scoped batch.

## R01–R16 record

| Rule | Scoped evidence and remaining gap |
| --- | --- |
| R01 | Checked family/prefix/length/count conversions, fallible builders, unchanged owned workers; strict GNU/musl0. Std/internal allocation/fault/close and whole panic-free certification remain unproven. |
| R02 |18 pinned primary files, approved concrete boundary/alternatives; actual IPv6 mechanism/cost/adoption data remains root-owned. |
| R03 | Protocol/UAPI family/width/offset/prefix and semantic RFC3849 identities; original caller budgets, OS ports/interfaces. No measured number becomes runtime policy. |
| R04 | Explicit Family and actual SocketAddr-derived tag; existing typed states/errors/completion/window unchanged. Actual transitions/errors owed. |
| R05 | Existing native engine and private fixed-message seam, no duplicate manager/cache/polling/framework. |
| R06 | Two actual family callers/finite origins, shared encoder and assertion body; no generic topology/controller. |
| R07 | Source/flag/scope/offset and safety comments explain new ABI/namespace obligations. |
| R08 | No benchmark or4–5x claim; goal unmeasured. |
| R09 | New IPv6 fixture preparation, no inferred IPv6 defect or invented guest pass; root owns actual evidence before any remedy. |
| R10 | Signed source, pure before/after byte witness, compiler/commands/exits/frozen artifacts/source hashes/raw retained; real effects/mutations/restored/cleanup owed. |
| R11 | amd64 GNU pure/compiler and musl compiler only; host compile/other seven-target and KVM/HVF/WHP remain unverified here. Mac arm64 only. |
| R12 | Shared actual SocketAddr/native TCP seams, no installed deliverable/host root/config/pin/grant/crypto/product change. |
| R13 | Finite IPv6 experiment only; unsupported semantics/full mediation remain open. |
| R14 | No allocation/fault/CPU/retention/resource-return/performance claim; all campaigns root-owned. |
| R15 | Current f5/d634 full implementer readback, proposal approval, source first/light-only signed checkpoint; no children/main merge. |
| R16 | Own clone/target, prior/before/final source/evidence/binaries preserved; exact admission/floor, no bypass/deletion/local build/root-cache write. |
