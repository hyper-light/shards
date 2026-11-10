# Relay-absent IPv4 TCP fixture checkpoint

Status: runnable fixture only; **actual relay-absent guest outcome unrun**.
The coordinator owns mechanism selection and all real guests, guard mutations,
restoration, audits, profiling, benchmarks, chaos and sweeps. This checkpoint
changes no product guard, route installation, kernel/config pin or wire builder.
The ad17 checkpoint remains separate and clean; this branch starts at
`ad17d2546fc3c97c96a3f93943fcbf72364a5d53`. Signed source checkpoint
`d0d92e827b05c0308222c56d0f674632e250171a` is pushed on
`p/managed-network-relay-absent`; this evidence-only follow-up does not change it.

POLICY_READ implementer: canonical g121-main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Full AGENTS/R01–R16/evidence/CLAUDE/managed plan/current handoff read before work
and again after compaction. Owned scope is testguest managed_network/intercept,
the approved adjacent usage string, existing host managed_network fixture and
this experiment's docs. No subagents, main merge or other lane edits.

## Exact experiment and terminal interpretation

The additional finite selector is:

```text
managed-network intercept-tcp4 forward-relay-absent DEADLINE_MS MARK TABLE RULE_PRIORITY
```

Existing local/forward/local-loopback/local-self select RelayMode::Present;
forward-relay-absent selects Origin::Forward with RelayMode::Absent. All use the
same native sockets, wire rules, three NEW held guest namespaces, RFC5737 topology,
OS-assigned ports, caller inputs and one original absolute deadline. Target
records now include the explicit relay mode. No host/current namespace setup,
external shell actor, extra binary, cache or periodic probe is introduced.

The backend first accepts the baseline connection, receives exact SENT bytes
`5245515545535400ff` plus EOF, and the client receives exact BACKEND bytes
`4241434b454e4400ff`. The same experimental interception rule is installed while
the transparent listener exists. In absent mode the sole listener owner reads
its actual port and drops that listener **before** the fresh forward client
connection is created. No relay worker or accepted relay socket ever starts;
RelayListenerClosed records the port and relay_worker=NotStarted. The backend
worker remains owned until its completion is joined.

A second backend accept is an unmediated effect even if no complete request is
received. A second exact SENT+EOF receipt is a separate independent effect.
Report tracks both checked counters; receipts increments only after the existing
exact tcp_receive succeeds. The client checks BACKEND bytes in this diagnostic
attempt so a successful direct response supplies a decisive byte witness, rather
than losing it as a mismatch against RELAY. ClientObservation retains exact bytes
and actual tuple; ServerObservation retains actual backend receipt and tuple.

ClientCompletion explicitly separates BeforeConnect, ConnectCall and ConnectedIo.
Only errors returned while the existing connect_timeout call is being attempted
can classify as connect refusal or connect deadline; after it succeeds, errors
have ConnectedIo stage. ClientOutcome records stage, typed outcome, kind and raw
errno (or null). It never infers a network refusal from a setup/namespace failure.

| Actual observations | Fixture result |
| --- | --- |
| Any backend accepts or exact receipts past baseline1; or successful exact BACKEND reply | DirectBackendEffect, nonzero regardless of any client/worker error. |
| ConnectCall ConnectionRefused, joined successful backend completion, exactly1 accept and1 receipt; relay NotStarted/0 accepts/0 receipts | Eligible ContainedRefusal, RelayAbsentChecked and zero only after host validation. |
| ConnectCall TimedOut | Distinct Deadline; Inconclusive and nonzero. |
| BeforeConnect failure, ConnectedIo error, any other client error, unknown/missing report, failed worker completion | Inconclusive and nonzero unless a direct effect already makes it decisively red. |

The host test checks accepted==1 AND receipts==1 independently, relay0/0,
joined=true, the connect-stage refusal, baseline bytes/receipt/peer identity and
RelayAbsentChecked before accepting guest status0. It parses records before the
status assertion so an actual backend effect remains visible on a failed guest.
No blanket error-as-containment pass is allowed. The coordinator alone may later
decide whether a deliberate dropping guard's connect deadline is acceptable,
with actual baseline liveness, complete ownership, no-second-effect observations
and a meaningful independent guard-removal direct-effect control. That acceptance
is not implemented here.

## Preserve reports across worker I/O errors

The coordinator's actual r227 at22ea removed the whole PREROUTING rule for each
new same-namespace target. Reported loopback result: backend2/relay0/joinedtrue,
cargo101. Reported local-self result: second exact SENT reached the backend and
a second ServerObservation was emitted; the backend worker then returned
NotConnected/ENOTCONN107. The old run path returned at backend_result? before
emitting Joined metadata. Both finish calls had already occurred; this is a
fixture evidence gap, not proof of an unjoined thread or a product routing defect.
Root retained `~/managed-intercept-22ea-local-mutations-v1`, including the original
wrapper assertion1, source byte-identical restoration and a later r230 restored
two-case gate0. These are coordinator-supplied earlier-source results, not this
checkpoint's passing gates. The raw transfer/central record is coordinator-owned.

At the existing worker seam Completion now carries Report together with its
io::Result. Workers return known counters even when later send/shutdown/logging
fails. Present mode calls both finish methods before propagating either result,
then emits Joined with accepts, receipts and typed terminal outcomes, emits
WorkerError kind/errno records, and propagates the original errors. Absent mode
joins the sole backend before record/error propagation and reports the relay
NotStarted state. ENOTCONN is **not suppressed**. Unknown join state/panic is an
error and never invents zero counts. Output failure can still prevent a complete
record; it cannot turn into a pass. Drop retains the existing eventfd wake and
join, with the original deadline as the failure bound. Real failure cleanup and
counter/receipt visibility still require coordinator replay.

## Sources, alternatives and unchanged authority

[Linux v6.18 nft_tproxy.c](https://github.com/torvalds/linux/blob/v6.18/net/netfilter/nft_tproxy.c#L62-L74)
looks up a listener when no established socket exists, and assigns a transparent
socket or sets NFT_BREAK. Its hash remains
`21ae51ad98808e74aa7a951664d090b07cb8e682e01d4b681161a77e73bf6248` in
[sources.json](sources.json). The current fixture chain accepts; this motivates
the missing-listener experiment but **does not establish actual guest behavior**.
No denial rule is added, chain policy changed or source-predicted red relabeled.

The pinned [Linux selftest](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/net/netfilter/nft_tproxy_tcp.sh)
and [TPROXY documentation](https://github.com/torvalds/linux/blob/v6.18/Documentation/networking/tproxy.rst)
remain the established local/forward comparison. Existing native baseline and
present-relay responders give distinct actual byte witnesses. A parser-only
missing-listener assertion would not observe a backend effect. Changing the guard
now would hide the required initial live red. A new relay/route manager or a
second socket engine would expand ownership needlessly. The reviewed solution is
one finite mode on the existing fixture seam, plus retained completion evidence.

Wire source is byte-identical to ad17, SHA256
`7ab793d0c1ffe7b817c0ffd83ce4a80064c8512177582d7b6fc0e1740af085f0`.
Caller-supplied deadline, mark, table and rule priority remain unchanged. Ports and
interface IDs come from real objects; protocol widths/reserved IDs retain their
pinned UAPI derivation. Baseline1/second attempt2 are semantic counts of the two
explicit calls, not a runtime cap. No numeric production constant is introduced.

## Actual light checks and identities

Private Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, exact admission
4CPU/8GiB/20GiB; private sync directory
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-network-relay-absent-542df5a032`,
target `~/managed-network-relay-absent-target`. Target began as a read-only reflink
of this lane's inactive local-targets target. Root and other lanes' targets/clones
were untouched. Rust1.98.1, Fedora44/amd64, host kernel6.19.10; exact compiler,
source and binary identities are in the raw logs. No timing instrumentation.

Final r83/v4 passed **5/5 pure tests**, strict GNU clippy0 and musl clippy0.
Tests cover finite mode/routing inputs, strict absence verdict/error stages and
unchanged wire/ACK vectors. No sockets, namespaces or VMs were exercised. GNU
unit executable SHA256
`e47e633a153016b84085eb76a596de390e683bbd71570c8fcfbf36ecc4621682` is frozen at
`~/managed-network-relay-absent-proof/v4/shards_testguest-975e61748c61ff31`.
Compiler logs show an actual rebuild after owned package clean/source touches;
source hashes identify the uncommitted patch atop ad17 without pretending ad17
alone contains it. Final source identities: [relay-absent-sources.json](relay-absent-sources.json).

[relay-absent-final-light.tar.gz](relay-absent-final-light.tar.gz), SHA256
`09ea9b1c714a8ef44ec410669efef3fce533a45081eb0029fd5f75307ab13481`, contains13
verified raw command/log/exit/environment/source/frozen-binary records and its
internal manifest. Expanded copy:
`/Users/adalundhe/g121-managed-network-relay-absent-final-light`.
[relay-absent-light.tar.gz](relay-absent-light.tar.gz), SHA256
`5e50548c917277ba1b8478636458438a41af01c0d7a66baaf4898643e340c6bf`, preserves39
raw records of earlier r79/v1, r80/v2 and r81/v3 intermediate patches. Those pass
results do not replace the final v4 check; their binaries remain in proof/v1–v3.

Exact light invocation (private remote target, no guest):

```sh
export CARGO_TARGET_DIR="$HOME/managed-network-relay-absent-target"
export RECORDS="$HOME/managed-network-relay-absent-light-v4"
export CC_x86_64_unknown_linux_musl="$PWD/scripts/zig-cc"
export AR_x86_64_unknown_linux_musl="$PWD/scripts/zig-ar"
export ZIG_TARGET=x86_64-linux-musl
bash docs/research/measurements/managed-network-intercept/run-light.sh
```

The driver records each actual cargo command and exit. Local rustfmt, bash syntax,
whitespace and check-development-policy pass; commands/exits are retained in
[relay-absent-local-checks.txt](relay-absent-local-checks.txt). The host fixture changes are syntax/
format checked only, **not compiled/run** here. No local Cargo/compiler/C build;
observed local free33478504KiB remained above30GiB. No source/evidence deletion or
headroom bypass. No active job remains.

## Owed coordinator execution

Root must review/compose this patch, compile the affected host fixture/parser and
run the actual relay-absent forward guest before choosing any remedy. Also owe
all four present-variant regressions, relevant independent rule removals on the
new completion reporter, restored runs, failure/cleanup/resource observations,
other supported targets/hardware and all cost/security coverage. Earlier central
results are not inherited passes. No IPv6/UDP/forwarding-relay/grants/FD-transfer,
full-route containment, leak-free or performance promise follows from this fixture.

Inside the unchanged4CPU/8GiB/20GiB coordinator admission, with a source-fresh
private target, retain all existing explicit SHARDS_NETWORK_BOOT_MS/CASE_MS/
MEMORY_MIB/CPUS and SHARDS_INTERCEPT_MARK/TABLE/RULE_PRIORITY inputs. Use the
existing reviewed absolute experimental artifacts (never production-pin fallback):

```sh
export SHARDS_KERNEL="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
export SHARDS_NETWORK_EFFECTIVE_CONFIG="$HOME/managed-kernel-tproxy-5d23-v1/out/config-6.18.48-x86_64-managed-tproxy"
export SHARDS_NETWORK_CONFIG_PROVENANCE='coordinator r122 pinned 5d23 build and two-symbol overlay; supplied build-output config'
export SHARDS_INTERCEPT_ORIGIN=forward-relay-absent
export SHARDS_NETWORK_RECORDS="$HOME/managed-intercept-reviewed-relay-absent"
bash docs/research/measurements/managed-network-intercept/run.sh
```

Kernel SHA256 `97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae`;
config SHA256 `fb1a17c68b211c72fddec827313bbad0019b1165d43cc311ca2582ff91af0f8d`.
The driver retains supplied identity/provenance without claiming inputs establish
built config association. This command is **owed, not executed** by this lane.
A direct second backend accept/receipt or exact BACKEND success is the expected
required red condition if observed; retain it and do not weaken the expectation.

## R01–R16 record

| Rule | Scoped proof and remaining limit |
| --- | --- |
| R01 | Checked counter arithmetic, typed errors, owned worker joins; strict GNU/musl clippy0. Std/internal allocations, thread-runtime/kernel failures and implicit panic paths are not certified exhaustively; no production panic-free claim. |
| R02 | Pinned primary Linux/selftest/docs/UAPI and existing seam; native baseline/present alternatives above. Actual missing-listener data and any guard/mechanism adoption remain root-owned. |
| R03 | No production numeric change; two explicit attempts, OS ports/interfaces and caller budgets. Existing ABI/protocol/RFC5737 derivations retained. |
| R04 | RelayMode, client stage/outcome, Completion/WorkerOutcome and AbsenceVerdict make known effects/error/unknown states explicit; runtime transitions owed. |
| R05 | Existing deadline/socket/eventfd/worker seam reused; no cache, repeated probe, duplicate engine or new manager. |
| R06 | One finite forward-absent mode; focused completion/verdict helpers avoid interleaving error propagation with evidence loss. |
| R07 | Comments explain sole listener owner, close-before-connect, unknown counts and preserved error obligations. |
| R08 | No competitor/distribution/4–5x claim; target unmeasured. |
| R09 | No product fix inferred. Root r227 reproduced only the reporting gap after actual direct effect; actual no-listener outcome remains unrun. |
| R10 | Source hashes, exact light commands/logs/exits/frozen artifacts retained; root actual red/green/mutations/restored/cleanup explicitly owed. |
| R11 | amd64 GNU unit/compiler and musl compiler evidence only; host compile, other seven-target checks, arm64 and real KVM/HVF/WHP remain unverified here. Mac arm64 only. |
| R12 | Shared fixture/native seams; no host root, extra installed deliverable, kernel/config/pin/crypto or production architecture change. |
| R13 | Small fixture-only absence/progress evidence; no forwarding relay or broader contract claim. |
| R14 | No profiling/benchmark/allocation/page-fault/resource-return claim; root owns all campaigns. |
| R15 | Current implementer receipt, approved scope, light checks, signed named paths; no subagents/main merge. Coordinator reviews/composes and executes all heavy/final gates. |
| R16 | Private exact-envelope target/clone and preserved archives/binaries; no root/other-lane cache writes/deletion or guard bypass; local floor maintained/no compiler. |
