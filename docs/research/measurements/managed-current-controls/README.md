# Current shared-client control adaptation

Actual follow-up: [requested current-caller proof](RESULTS.md) passed the fresh
normal caller/matching guest builds, two real KVM header-FIN channels, one native
canonical-Aborted Subscribe case, seven pure checks and scoped Clippy. The173
original raw files are retained in the verified archive. Source-stage pending
gates below preserve their original chronology; checkpoint is not lane release.

Base `28e147c4dda846ca023020e03bc9b00eb2e58e94`, canonical main33df95b08,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Assigned implementer/adversarial01a1c5d2ee5871a2a9a9cdc9f56793f0;
coordinator01a148884a4770fdaf0111f3f792a248. Full policy/evidence/CLAUDE/plan/
handoff reread before edits. The superseded3f7 scope was never built/tested;
28e changes only ROOT's metrics-test nested-if layout. Its exact delta is retained.

## Exact source scope and reuse

The new `managed_output/current_controls.rs` leaf imports the moved shared
Ask/Event/Observer/Metadata through the existing parent. Its four children are
byte-identical to frozen2475533fa. Only leaf imports/include paths adapt the
existing tested callers. The old source branches, binaries, original raw patch
and records remain frozen; their passing gates are not transferred to this base.

The shared `common/managed_capture.rs` adds the SAME approved test-only RawControl
and receipt sink, gated with cfg(test), to its sole existing Client. Every Client
initializer (one) sets the sink to None. One bounded raw command is admitted only
after the subscription pending buffer is empty and before any earlier raw command.
The original pending/partial-write/writable/read/finish path remains authoritative;
FIN follows all pending bytes. Actual Write/Read/Finish/WriteError receipts have
no protocol authority and do not prove backend application consumption. No extra
client/observer/manager/control queue/cache/protocol or transport is introduced.

`shared-client.patch` and `registration.patch` snapshot the applied current-source
diff for review; do not apply them twice. Original frozen patches are retained
unchanged alongside them. `source-scope.json` gives all owned source/patch hashes;
`frozen-leaf-adaptation.diff` gives the exact caller adaptation. No production,
ABI, metrics, other fixture or protected ROOT cache source is edited.

Existing primary/alternative reasoning remains in the preserved terminal-contract
and seam-proposal records. Current Client and remote Service/parser/Endpoint
contracts were inspected at this base: a copied driver, raw UDP or production
state injection would bypass the actual existing authenticated seam. This is a
caller adaptation of an approved seam, not a new architectural decision.

## Assigned finite campaign

Full normal carried-helper `managed_output` caller and matching own static guest
must compile before actual cases. Two new owner runs select `control-header-only`
on stdout/stderr after a real canonical Open. Actual whole-header Write and FIN
receipts, typed Closed, exact held credit/no future record, fresh controlled
neighbor fd1/fd2/main bytes and owner stop/remove/join/FD witnesses stay strict.
The native case selects existing Subscribe after supplied canonical Aborted,
stdout/positive errno5, using actual Endpoint/parser/transport. It requires
Protocol, exact supplied cause/Finished cursor, returned owned payload, observer
retirement before owner abort and no finish-after-abort resurrection. Native
cause is supplied, not a kernel-observed guest failure or daemon metadata proof.

VM test: `tenant_exit::current_controls::invalid_application_ack_after_record_preserves_credit_neighbor_and_main`.
Native test: `tenant_exit::current_controls::native_terminal::canonical_aborted_resubscribe_preserves_errno_and_terminal_credit`.
Existing explicit input names remain SHARDS_ADVERSARIAL_ and
SHARDS_ADVERSARIAL_NATIVE_. Caller supplies positive phase/work<=240000ms,
cleanup15000ms, outer255000ms, new absolute records and exact source revision;
all fixture deadlines remain anchored to their first Instant. Direct caller
enforces the outer bound. No polling/retry/reset, replacement endpoint or
weakened assertion. Runtime headroom refuses rather than silently reducing work.

Runner8fadc1ec213332e16cb0d915a3fb913f, NEW private
`/home/adalundhe/managed-current-controls-28e-v1/{source,target,cargo-home,zig-global-cache,zig-local-cache,runtime-cache,artifacts,records}`.
Build/campaign2CPU/4GiB/24GiB, j4, build<=3600s; each case<=240s+15s cleanup.
Linux amd64 GNU/KVM only; no heavy local build/target or ROOT source/cache reuse.
Own earlier experimental kernel may be copied read-only with verified digest;
its scope is explicit rather than presented as the stock product kernel gate.

## R01–R16 at source checkpoint

| Rule | Evidence / remaining limit |
| --- | --- |
| R01 | Test-only assertions, no production path changed; current full build/Clippy pending. |
| R02 | Existing shared Client/Observer/Service and frozen real callers inspected; primary contracts/alternatives retained. Current live evidence pending. |
| R03 | Real encoder/parser bounds and observed cursors; user-supplied positive finite budgets. No production constants or capacity changes. |
| R04 | Typed selection/cause/credit/canonical terminal/transport/owner states remain distinct and strict. |
| R05 | Existing pending/partial writes/readiness/finish and wake-only callback reused; no added polling/cache/manager. Receipt sink is test evidence, not state authority. |
| R06 | Sole shared implementation plus unchanged focused fixture children; no framework or second adapter. |
| R07 | Comments/evidence explain original-source association, local receipt limits and supplied-cause uncertainty. |
| R08 | No benchmark/performance/4–5× claim. |
| R09 | No product bug/fix inferred; current actual gates pending. |
| R10 | Source/patch hashes and original frozen records preserved; current commands/build/case/raw/cleanup records to follow. Relevant product guard mutations ROOT-owed. |
| R11 | Assigned Linux amd64 GNU/KVM; other libc/OS/VM/seven-target validation ROOT-owed. macOS arm64 only; paused WinARM untouched. |
| R12 | Existing shared ABI/transport/platform and moved common fixture; no duplicate OS implementation or installed binary. |
| R13 | One selected fresh owned run/endpoint per case; original strict cleanup and deadline invariants preserved. |
| R14 | No profiles/benchmarks or allocation/fault/CPU/kernel/parent accounting claims; full population/resource return unknown. |
| R15 | Assigned bounded current callers only. No children/goals/autoplan/main merge; ROOT owns audits/profiles/benchmarks/heavy integration. Checkpoint is not lane release. |
| R16 | New private admitted remote scope; no active local target/cache, >=30GiB floor, no deletion. All earlier sources/evidence/frozen binaries preserved. |

Still owed: current full caller/helpers/guest build, actual three cases, scoped
pure checks/Clippy, then ROOT's shared-consumer/current combined/matrix/mutation
review, daemon fault/metadata order, population/kernel evidence and managed-plan
acceptance. Held helper malformed-control, worker-result allocator and cotenant
theft/ptrace campaigns remain held and are not rerouted here.
