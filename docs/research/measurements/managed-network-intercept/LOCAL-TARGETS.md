# Same-namespace IPv4 TCP experiment targets

Status: scoped fixture extension, **actual same-namespace guest behavior unrun**.
Base combined `0e5f175fb39330ac5d89048fc5a1ad68af5bdb50` (same P4 source1f891
as pushed fed0102c). Current canonical policy g121-main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Role implementer; full AGENTS/R01–R16/evidence/CLAUDE/plan/current handoff read
before edits, scoped receipt returned. Coordinator owns all audits/profiling/
benchmarks/heavy real E2E/chaos/stress/sweeps, source review and final merge.

The coordinator reports actual local+forward IPv4 guest passes at fed0102c,
independent local OUTPUT-rule and forward full-PREROUTING-rule removals with
actual BACKEND second accept/relay0/cargo101, and both restored passes. Their
central raw checkpoint is forthcoming; no agent rerun or new-source pass is
inferred from those results. This extends G05 coverage preparation; it does not
close same-namespace mediation or adopt a product mechanism.

## Two explicit variants at the existing seam

| CLI origin | Client namespace | Native backend listener | Transparent responder |
| --- | --- | --- | --- |
| local | New router | Existing new backend peer,192.0.2.2 | New router |
| forward | Existing new client peer | Existing new backend peer,192.0.2.2 | New router |
| local-loopback | New router | Same new router,127.0.0.1 | Same new router |
| local-self | New router | Same new router,configured192.0.2.1 | Same new router |

The original fixed three-object topology remains, with no generic topology layer
or extra namespaces. The new variants place their real backend listener and
client in the held NEW router namespace; the other two topology peers remain
fixed setup objects. `Namespaces` continues naming those three objects, while
new typed `Target` JSON separately reports the actual bound listener destination
and actual client/backend/relay role namespace inodes. The host assertion checks
that the selected roles share the router for both new variants and compares this
destination to baseline/server/client/accepted original tuples. These identities
come from held namespace FDs and a listener created only after successful setns;
they are not inferred from user labels or notification text. Native bind at the
own address must succeed before its identity is emitted. All setup still occurs
only in fresh guest namespaces, never host/current namespace administration.

Ports remain OS-assigned. Existing native socket, baseline BACKEND vs distinct
RELAY byte witnesses, server receipts/counts, original tuple checks, one caller
deadline, cancellation and Running/Joined RAII are reused unchanged. The policy
method now accepts the **actual listener SocketAddr**, checks IPv4, and derives
its /32 local route from destination.ip(). The old remote BACKEND constant is
not used when the listener target differs. OUTPUT route marking applies to all
three local variants; forward keeps its existing PREROUTING path. The parent
usage string and ignored host test accept exactly these four finite choices;
the root argv diagnostic and native engine/parser semantics remain unchanged.

PREROUTING traversal and return-path compatibility for loopback/own-address are
**the experiment**, not conclusions from adding rules or reading kernel docs.
Because these addresses already have a local FIB route, removing OUTPUT alone
may still intercept. Do not copy the prior local-to-remote mutation promise.
Root must choose the actual relevant independent guard, observe client/server
bytes and actual backend/relay counts, restore source and repeat the guest.
A refusal or missing alert alone is not a direct-routing proof.

## Primary semantics, alternatives and bounds

The pinned Linux v6.18 selftest, TPROXY documentation, nft_tproxy.c and exact
netlink/fib UAPI sources/hashes remain [sources.json](sources.json), with the
[initial decision](README.md#sourcealternatives-and-small-implementation-seam).
The plan's G05/P4 specifically distinguishes loopback and own-address traffic
from routed peers. Linux's PREROUTING restriction does not prove either new case
works; actual guest data decides. Reusing the fixed setup/role selection was the
smallest extension; a second engine or generic topology layer would add state
without required variants. No new production API, cache, relay forwarding,
IPv6/UDP support, kernel pin/config, host root or crypto change is introduced.

127.0.0.1 uses Rust's Ipv4Addr::LOCALHOST;192.0.2.1 retains the existing RFC5737
fixture address already assigned through acknowledged setup and required by the
native bind. Both are semantic test targets, not learned runtime limits. /32 is
the exact IPv4 host route; register/field/alignment values retain UAPI derivation.
Deadline, mark/table/priority stay explicit caller inputs. There is no new timer,
resource cap or copied measurement constant.

## Actual light results and runnable coordinator obligation

One admitted Fedora runr77, exact4CPU/8GiB/20GiB, private clone
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-network-local-targets-e49527448b`
and `~/managed-network-local-targets-target`, passed **4/4 pure tests**, strict
x86 GNU clippy0 and musl clippy0. Tests cover finite origin parsing and conflicting
inputs, ACK handling, and exact loopback/self IPv4 destinations in selector/route
encoding; no socket, namespace or VM was exercised. Compiler logs show actual
rebuild after package-clean and source touch. No timing instrumentation/counters.
Target began as a read-only reflink of this lane's inactive private P4 target;
root and other lane clones/caches were untouched. Mac stayed above30GiB, with
no local compiler or cleanup. Local rustfmt, shell syntax, whitespace and policy
checks passed. No active job remains.

[local-targets-light.tar.gz](local-targets-light.tar.gz), SHA256
6cb524effc0a3b1c0cb0b401e180bc675c4a90c6fe48d55b9e1afd97e17e6504,
contains13 verified raw command/log/exit/environment/source/binary records and
an internal manifest. Expanded copy:
`/Users/adalundhe/g121-managed-network-local-targets-light`; frozen unit executable
and SHA record: Fedora `~/managed-network-local-targets-proof/`. Final source
identities are recorded in [local-targets-sources.json](local-targets-sources.json).
The host test changes were syntax/formatted only here, not compiled/executed.
GNU/musl checks compile guest code, not real guest behavior or all seven targets.

Root owes host compilation/parser checks, actual local-loopback and local-self
boots, actual relevant independent guard removal/byte effects, restoration,
resource/cleanup observation and original local/forward regression on the combined
revision. All other host/architecture/VM, exhaustion, cost and protocol coverage
remain unverified. Use the unchanged [coordinator driver](run.sh), source-fresh
private target and existing experimental kernel/config, with all prior supplied
SHARDS_NETWORK_* budgets/identity inputs and explicit mark/table/priority. Execute
one fresh record directory per case, inside the unchanged4CPU/8GiB/20GiB envelope:

```sh
export SHARDS_INTERCEPT_ORIGIN=local-loopback
export SHARDS_NETWORK_RECORDS="$HOME/managed-intercept-reviewed-loopback"
bash docs/research/measurements/managed-network-intercept/run.sh
# Repeat separately with origin=local-self and a different new records directory.
```

This is an owed root command, not an executed/passing campaign. Existing
experimental SHARDS_KERNEL and supplied config/provenance are required; no
production-pin fallback. The shared driver refuses zero-test/no-observation
success. Root selects and records all actual budgets/IDs and mutation scope.

## R01–R16 checkpoint record

| Rule | Scoped proof and remaining limit |
| --- | --- |
| R01 | Added role matches/checked IPv4 destination and existing errors; GNU/musl strict clippy0. Existing std/internal allocation/close/fault limitations remain, no panic-free/exhaustion certification. |
| R02 | Existing pinned kernel/docs/UAPI and reviewed plan; prior root peer results distinguished from two unrun local-target experiments. No product design adoption. |
| R03 | Semantic loopback/existing RFC5737 target, OS ports/interfaces and caller budgets; no new production numeric policy. |
| R04 | Two explicit Origin variants and Target typed JSON distinguish actual roles from fixed topology; lifecycle/typed errors retained. |
| R05 | Same private builders/socket/deadline/RAII seam, no duplicate engine, topology framework, cache or periodic wait. |
| R06 | Finite four-origin role match; no dynamic topology manager or extra namespace population. |
| R07 | Comments explain role identities versus topology and actual destination authority; source rationale above. |
| R08 | No measured competitive or4–5x claim. |
| R09 | New fixture coverage preparation, no suspected product defect/fix or invented same-namespace pass. |
| R10 | Four pure tests/compiler/source identities/raw logs retained; actual new guest/guard effects and restored regressions explicitly owed. |
| R11 | Guest amd64 GNU/musl compiler evidence only; host test compile, arm64 and seven-target/real hardware gates remain root-owned. macOS arm64 only. |
| R12 | Same native fixture seam, no production routing/pin/config/crypto/host-root/installed-binary change. |
| R13 | Small explicit target extension only; no forwarding relay, IPv6/UDP/grants/FD-transfer/full mediation claim. |
| R14 | No profiling/benchmark/allocation/resource return claim; root owns all those campaigns. |
| R15 | Current f5/d634 implementer receipt; source/light checks only, signed named paths, no subagents/main merge. |
| R16 | Private Linux clone/target/read-only cache source, exact admitted envelope; Mac floor preserved, no compiler/deletion here, root cache untouched. |
