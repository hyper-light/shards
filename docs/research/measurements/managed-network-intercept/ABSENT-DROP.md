# Selected-flow fallback DROP experiment

Status: **experiment-only source; guarded guest behavior unrun by this lane**.
Signed source `eb0b1997a1902ddbaba31234d8825542cb162149`, branch
`p/managed-network-absent-drop`, separate worktree from preserved
`2fac2250c6f40228cf06f6bda3ce81d90547043f`. Only
`crates/testguest/src/managed_network/intercept/wire.rs` changes runtime fixture
code. The existing relay-absent predicate, host fixture, native engine, OUTPUT,
product grants/routes, kernel/config pin and production code are untouched.

POLICY_READ implementer: full current g121-main AGENTS/R01–R16/evidence/CLAUDE/
managed plan/current handoff, revision `f5cec365d2e435afae5e6f613c92d9d3759ef60f`,
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Owned scope: the private wire seam, its pure units and these experiment docs.
Coordinator alone owns all audits/profiling/benchmarks/heavy guests/chaos/sweeps,
source review, combined validation and merges. No children or main merge.

## Actual red that authorizes this experiment

Coordinator r279 at exact combined `37a62c83586876ad090194ef8959b4500a3a01ec`
reported all four present variants0. Forward-relay-absent returned cargo101 with
actual ConnectedIo/Success/Backend and full BACKEND bytes, Joined
backend_accepted2/backend_receipts2/relay0/Stopped/joinedtrue and
AbsenceVerdict DirectBackendEffect. Raw remains root-owned at Fedora
`~/managed-combined-37a6-runtime-v1`. This is the real missing-listener direct
effect, distinct from the earlier source hypothesis and r227 reporting gap.
The coordinator also reports task recovery/held-cancel0 and r280 seven-target0
at37a; these results are **not inherited passes** for this new guard source.
This lane did not rerun or audit that campaign. Original raw/inputs remain with
root for central retention.

## Pinned semantics and alternatives

[Linux v6.18 nft_tproxy.c:62–74](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_tproxy.c#L62-L74)
sets NFT_BREAK when no suitable transparent socket is available.
[nf_tables_core.c:255–290](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nf_tables_core.c#L255-L290)
ends expression evaluation on a non-CONTINUE verdict, resets BREAK and advances
to the next rule; ACCEPT returns, while DROP terminates with a netfilter drop.
The [UAPI verdict contract](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/netfilter/nf_tables.h#L48-L64)
defines BREAK as ending the current rule. Thus putting DROP after TPROXY within
that same rule would skip the denial on the missing-listener path.

The next PREROUTING rule therefore repeats the **same selector()** for the actual
bound IPv4 destination, TCP protocol and destination port, then emits a typed
immediate NF_DROP. The first rule retains TPROXY, mark and ACCEPT. Both rules use
the existing NLM_F_APPEND encoding and one atomic batch. A successful first rule
exits before the fallback; selection mismatch retains the prior base-chain
ACCEPT and existing OUTPUT selection. Those are encoded source properties;
actual matching, unmatched traffic and successful-route compatibility need root
runtime observation. This fixture still administers only its NEW guest namespaces.

[netfilter.h:9–10](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/netfilter.h#L9-L10)
defines DROP0 and ACCEPT1. The existing verdict-register immediate encoding now
accepts the private two-variant Verdict enum, sharing identical ACCEPT bytes.
[nft_immediate.c](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_immediate.c#L16-L40)
and nf_tables_core's basic type registration establish the already-used immediate
seam; the added DROP requires no new expression/module/config feature here.

| Alternative | Compatibility, authority and cost tradeoff |
| --- | --- |
| Separate exact-selector DROP (chosen experiment) | Retains existing selected successful route and unmatched policy; one extra rule/selector and silent denial. Retries and finite deadline residence may cost more than prompt refusal. No measured CPU/memory/latency claim. |
| Separate exact-selector REJECT | Can generate TCP reset/ICMP notification before dropping; changes error/packet behavior and adds reject implementation/config dependencies. Linuxv6.18 permits PREROUTING for reject; hook support is not a reason to rule it out. No kernel/config change or module installation is authorized in this batch, and REJECT is not introduced merely to turn the current refusal predicate green. |
| Base-chain DROP | Changes all unmatched traffic and return-path behavior; wider authority/compatibility effect than the reproduced selected flow. Rejected for this narrow experiment. |
| DROP in the TPROXY rule | BREAK skips it on the relevant path. Does not supply the required next-rule denial. |
| Runtime disable/health probe | Adds state/polling and does not establish denial at packet evaluation. Not introduced. |

The REJECT comparison uses pinned
[nft_reject.c](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_reject.c#L26-L37),
[nft_reject_ipv4.c](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/netfilter/nft_reject_ipv4.c#L17-L35),
[core Kconfig](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/Kconfig#L540-L552)
and [IPv4 Kconfig](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/netfilter/Kconfig#L39-L42).
NFT_REJECT_IPV4 selects NF_REJECT_IPV4; the IPv4 evaluator sends the chosen
notification and then drops. The actual experimental config was not audited for
those additional symbols here; no assertion that they are absent is made.
Any adoption/cost comparison remains coordinator work. Download identities and
an explicit archival429 failure for nft_reject_ipv4.c are preserved in
[absent-drop-primary-sources.json](absent-drop-primary-sources.json); that file
was successfully read through the primary-source browser. Eight other complete
pinned source files remain at `/Users/adalundhe/g121-managed-network-absent-drop-primary`.

## Capacity, waits and unchanged acceptance

The old picked7 reservation is replaced by a checked sum of the actual initial
message array length, optional local OUTPUT array length and ending array length.
The vector reserves that exact count fallibly before extending any array. This
also covers the additional rule in all variants; no unreserved message-vector
growth occurs. Existing byte builders reserve before appends. Count arithmetic
and ABI errors remain handled; std/internal allocator/kernel fault paths are not
certified exhaustively, and this is no allocation-free or panic-free claim.

No new timer/cache/manager/interface framework is added. The private pure batch
helper is the same encoder used by exchange, enabling inspection without socket
or namespace setup. Existing eventfd readiness, worker ownership, exact receipt
counters and original caller deadline are unchanged. The absent-mode decision
continues to accept only its narrowly established connect-refusal predicate.
**A connect deadline or worker error remains inconclusive and nonzero.**
A guarded run may therefore still produce cargo101 at the original shared deadline.
That outcome alone does not decide containment or disprove the guard. Root must
observe actual client stage/kind/errno, baseline liveness, no second backend
accept AND exact receipt, relay0/0 and complete joined ownership before deciding
bounded terminal proof semantics. No timeout increase or error-as-success change
has been made. Actual direct effects remain red regardless of client error.

## Actual light checks and retained identities

One final private Fedora r85/v1, exact4CPU/8GiB/20GiB, passed **6/6 pure tests**,
strict amd64 GNU clippy0 and musl clippy0 with Rust1.98.1. No sockets, namespaces,
VMs or mutations were executed. The new unit decodes actual assembled messages
for all five finite selectors, checks separate appended pre rules, identical
TCP/destination/port match bytes, TPROXY+mark+ACCEPT before DROP, verdict-register
byte order, unchanged base-chain ACCEPT and local OUTPUT. Existing strict
absence-stage and wire/ACK tests remain in the six-test filter. This does not
simulate Linux evaluation or prove real packet behavior.

Private synced clone:
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-network-absent-drop-95bb6088df`.
Reused this lane's own inactive `~/managed-network-relay-absent-target`; no root or
other-lane clone/cache write. The driver cleans only the owned testguest package
and touches guest source to force freshness after sync, with actual compiler logs
and source hashes. Logs name base2fac plus the checked uncommitted wire hash;
[absent-drop-sources.json](absent-drop-sources.json) binds it to signed eb0b1997.
No counters/timing instrumentation. Final frozen GNU unit binary SHA256
`06e8ff8c7d9a8c0888c0502712a4af830749f0e92fdfbd7b80a3facb42f0c77e` at
`~/managed-network-absent-drop-proof/v1/shards_testguest-975e61748c61ff31`.

[absent-drop-light.tar.gz](absent-drop-light.tar.gz), SHA256
`bd4f091b2ae3ecc513b9f7438735eaa8adfabdf5cfdf90bc6cae43ce7f612be8`, contains13
verified command/log/exit/compiler/source/binary records and an internal manifest.
Expanded copy: `/Users/adalundhe/g121-managed-network-absent-drop-light`.
The unchanged [run-light.sh](run-light.sh) was invoked with this private target,
RECORDS=`$HOME/managed-network-absent-drop-light-v1`, CC/AR musl pointing to this
checkout's scripts/zig-cc and zig-ar, and ZIG_TARGET=x86_64-linux-musl. Exact cargo
commands/exits are in the archive. Static format/shell/policy/whitespace checks0
are [retained](absent-drop-local-checks.txt). No local compiler/Cargo build; Mac
free40285520KiB at static check, above30GiB. No deletion/admission bypass or active job.

## Owed coordinator run

Root reviews/composes source, then reuses the existing explicit kernel/config/
provenance, private target, SHARDS_NETWORK_BOOT_MS/CASE_MS/MEMORY_MIB/CPUS and
SHARDS_INTERCEPT_MARK/TABLE/RULE_PRIORITY inputs, inside the unchanged
4CPU/8GiB/20GiB admission. The experimental artifact identities and setup remain
in [RELAY-ABSENT.md](RELAY-ABSENT.md#owed-coordinator-execution). Exact next entry:

```sh
export SHARDS_INTERCEPT_ORIGIN=forward-relay-absent
export SHARDS_NETWORK_RECORDS="$HOME/managed-intercept-reviewed-absent-drop"
bash docs/research/measurements/managed-network-intercept/run.sh
```

This is owed, not an executed/passing guest. Preserve all terminal records even
if the unchanged predicate returns nonzero. Root also owes all four present
regressions, actual unmatched-traffic controls, independent removal of **only the
new fallback rule** observing the direct backend effect, byte-identical restore,
joined-worker/failure/resource evidence and relevant target/hardware/cost gates.
No fixture acceptance change precedes those observations. No product mechanism,
IPv6/UDP/forwarding relay/grants/FD-transfer/full-route mediation is adopted.

## R01–R16 checkpoint

| Rule | Scoped evidence and remaining gap |
| --- | --- |
| R01 | Checked actual count, fallible reserve/byte appends, typed verdict; GNU/musl strict0. Std/internal allocation/failure/close limits above remain unproven; no whole-path panic-free claim. |
| R02 | Pinned evaluator/UAPI/immediate/reject/Kconfig sources and established alternatives; actual root r279 red precedes experiment. Guarded behavior/cost/adoption awaits root. |
| R03 | UAPI NF_DROP/NF_ACCEPT, existing selectors and actual message-count derivation; no timer/cap/default copied from measurement. |
| R04 | Typed verdict at existing wire seam; existing client/worker state, counts and strict error predicate unchanged. |
| R05 | One private batch encoder used by real exchange and pure checks; no cache/poll/framework/parallel manager. |
| R06 | One subsequent exact-match rule; finite origins and original OUTPUT structure, no topology expansion. |
| R07 | Comments explain BREAK next-rule obligation and pinned UAPI verdict values. |
| R08 | No performance ratio/4–5x achievement; comparison costs source-derived directions only. |
| R09 | Exact coordinator r279 direct-effect red recorded; no inferred guarded pass or product defect/adoption. |
| R10 | Signed source, primary/source hashes, raw commands/logs/exits/frozen identity; actual guarded/mutation/restored/effect/cleanup evidence explicitly owed. |
| R11 | amd64 GNU unit/compiler and musl compiler only; other seven-target and KVM/HVF/WHP/runtime gates unverified here; macOS arm64 only. |
| R12 | Same fixture/private native seam; no installed binary, host root, production/kernel/config/pin/crypto change. |
| R13 | Small packet-rule experiment only; containment/full mechanism remains incomplete. |
| R14 | No profiling/benchmark/allocation/fault/retention/CPU/resource-return claim; root owns all campaigns. |
| R15 | Full f5/d634 implementer receipt, exact owned scope and light tests; signed named source first, evidence separate, no children/main merge. |
| R16 | Own clone/target and frozen prior evidence preserved; exact admission/floor, no root cache writes/deletion/headroom bypass/local build. |
