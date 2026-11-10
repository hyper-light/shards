# Joined-run identity and current multi-tenant closing gate

This is a tested partial management batch, not completion of P0–P13/G01–G25.
Product/test source is `0ae89529036e4e30dba639169844f1bcb5ba84da`.
The production identity correction is `c13863223`; subsequent crate changes
through0ae are the reviewed shared observer/tenant fixtures and testguest input.
Closing commits change only evidence and documentation. Earlier source results
remain associated with their original revisions.

POLICY_READ: ROOT coordinator, current main a920c85b5; complete AGENTS.md,
development-rules/evidence, CLAUDE, managed plan and handoff read. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
ROOT owns integration, audits, profiles, heavy gates and main merge. Implementers
and assigned adversarial agents retain their separate private scopes. Windows
ARM stays paused; no old goal or held campaign is resumed by this batch.

## Behavior and authority

A joiner shares its provider's real VM while retaining a distinct workload/run.
The existing tracked `joined` relationship supplies the provider identity; no
request-file walk, depth limit, timer, identity cache or additional grant exists.
The private METRICS builtin receives two checked canonical IDs. OTLP resources
carry separate `shards.run.id` and `shards.microvm.id`. A joiner still exports
only its own workload. The [identity decision](../managed-otel/joiner-identity/README.md)
records pinned OTel resource/proto sources and rejected duplicate-authority paths.
The restore attribute remains VMM lineage count, not management epoch.

Current tenant fixtures use the one canonical Client/Observer and actual records.
The release caption is `OwnerReleasedNeighborAfterGateEof`, the observed gate
event. It no longer falsely claims the actor's two streams already ended.
Their independent offsets/terminal/neighbor assertions are unchanged. Inherited
domain streams do not identify an individual descendant writer, and pipe EOF
does not prove the whole population reaped. No such stronger claim is made.

## Actual before and after

`red-e37.py` first failed before metrics because `confined cat` deliberately stays
alive. Its131.69s harness failure and raw data are retained; it is not evidence
of a telemetry defect. `red-9be.py` uses the existing single-shot `slurp` reader.
On unchanged a920 product bytes, the real KVM test then fails101 in11.45s:
the actual boot IDs match, run IDs differ, but joiner OTLP claims its own run as
the VM. Owner/observer cleanup returns, FD7→7.

At c138, init metrics20 tests, strict caller lint, normal carried build and static
guest build pass. Original provider, new joiner and legacy code-mode KVM tests
each pass1/1,0ignored. The c138 seven-target gate and native remote15 tests pass.
These early results are retained separately from the final combined gate.

Root combined Linux0ae uses normal carried init/server/VM/network consumers,
matching static musl testguest and product kernel SHA256
`48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7`.
Runner: Fedora44 amd64, kernel6.19.10-300.fc44.x86_64; admission4CPU/8GiB/16GiB,
cargo-j4. Admission is not a hard cgroup or measured physical/kernel peak bound.

| Gate | Actual result |
| --- | --- |
| Policy, init metrics, native strict caller lint | 0; metrics20 passed |
| Normal carried managed-output/build consumers and static guest | 0 |
| Entire ordinary managed_output binary | 25passed,0failed,10ignored,52.28s |
| Selected normal tenant exit | 1passed,0ignored,2.90s |
| Selected inherited child writers | 1passed,0ignored,2.60s |
| Header-only FIN stdout / stderr | each1passed,0ignored,2.42s/2.22s |
| Native canonical-Aborted resubscribe | 1passed,0ignored,0.07s |
| Legacy code mode, original grants/effects/cancel assertions | 1passed,0ignored,4.56s |
| Mac ARM64 scripts/lint | all7 supported targets, GPU and format, exit0 |
| Mac native remote managed corpus | 15passed,0ignored |

The five explicitly selected managed cases above are drawn from ignored tests;
their individual invocations actually run them. The other ignored cases remain
unrun in this gate. No SKIP is accepted. No new HVF/WHP/ARM64-guest result is
inferred from compilation. Drivers: [linux-v2.py](linux-v2.py), [mac.py](mac.py).

First `linux.py` failed at checkout, before tests, because the old generated
profile lock would be overwritten by its now-tracked identical blob. The original
13938-byte lock was verified byte-equal and renamed into that failed run's record,
SHA256 `a2fdba6d5b9af71207b2debf43270c35b520b31fe789b143779652532f985d84`.
Only the output directory changed for v2. Neither file was deleted or relabelled
as a product-test failure; original checkout/fetch/preservation receipts remain.

## Independent OTLP consumers and identity mutation

The pinned official pdata1.71.0 decoder accepts the current first/second/provider
exports (4resources24points each) and joiner (1resource6points). Official
Collector0.165.0 accepts all four over its actual private loopback OTLP HTTP
receiver: HTTP200, no partial rejection/warning, named metric debug receipts,
graceful exit0 and controllerFD4→4 on each. These are test-consumer facts;
production HTTP export/delivery is not implemented by them.

Collector binary SHA256
`14e254a6a0880d9b6f94487658e49fc9f3b0e14c62e82253be78da08e8c1ddd8`;
decoder SHA256 `dc8281fbaa3f6e6b325958f706c31425d3f2f7783c639ebd2bce24c46dfee62b`.
The three current native/generated reference payloads have equal complete
official decoder receipts. Both distinct identity attributes are included.

[identity-closing.py](identity-closing.py) removes only provider selection,
substituting the joiner's own run ID. Normal build passes. The actual KVM export
contains that wrong VM ID, and the test fails101 at metrics.rs:245 with cleanup
FD7→7. The original wrapper then fails1 because it looks for the assertion in
stdout while panic text is in separately retained stderr. Its `finally` restores
the exact source. [identity-restored.py](identity-restored.py) independently
checks both raw streams, IDs/export and exact source restoration, then rebuilds
normally and runs the restored case:1passed,0ignored in12.31s, shared VM/distinct
run IDs, FD7→7. No test assertion was weakened or red repeated to obtain a pass.

A separate harness storage mistake also remains explicit: the reference shape
checker reused `.receipt.json`, overwriting the first official decoder stdout.
The overwritten outputs are not claimed retained. Only the official decoder was
rerun over byte-identical payloads into distinct `.official-pdata.receipt.json`;
commands/input/output hashes, equal receipts and the correction are retained.
Shape receipts and failed wrapper records remain unchanged.

## Current-source encoding costs and misses

Same pinned generated Rust request reference and fixed three-snapshot corpus as
the [original method](../managed-otel/profile/README.md), now with separate run
and VM attributes in BOTH encoders. Uninstrumented optimized binaries run9 blocks
of1000 per mode with rotated order. Separate100-call processes count actual Rust
System allocator calls. [profile-summary.json](profile-summary.json) and original
per-call raw data retain the full distributions and counters.

| Encoding | n | p50/p90/p99/max, µs | Observed alloc/realloc calls per request |
| --- | --- | --- | --- |
| Direct streaming | 9000 | 2.954 /3.368 /3.799 /154.418 | 0 /0 |
| Official generated request, compact buffer | 9000 | 8.665 /13.249 /16.110 /199.706 | 181 /15 |
| Official generated request, pretty buffer | 9000 | 16.770 /25.178 /29.049 /195.619 | 181 /17 |

Compact pooled-p50 ratio is2.933×; per-block ratios2.533–4.781×. This **misses4×**;
the old source's4.139× result is not reused. Pretty pooled ratio5.677×, block
range4.625–10.060×. Scheduling/noise remain visible: load at entry6.82/14.17/16.14
on32cores; no affinity/frequency lock or confidence interval is claimed. This is
an encoding comparison, not a complete SDK pipeline, HTTP transport or VM result.

All observed allocations are deallocated within each counting region; native
live2233→2233, compact2243→2243, pretty2242→2242. Native565 Write calls/request
versus1 buffered call are not syscalls: production has an existing BufWriter.
Minor faults total32/124/177 respectively; major faults0. All27 timing processes
report the same inherited ru_maxrss194492KiB: no incremental RSS/PSS advantage
can be inferred. Copies, C/native allocations and induced parent/kernel costs
remain unmeasured; no allocation/leak-free whole-operation claim is made.

## R01–R16 coordinator review

| Rule | Evidence and limit |
| --- | --- |
| R01 | Existing denied panic lints pass all7. New ID argv copies reserve fallibly; serializer retains checked conversions/Write errors. Test assertions/panics are confined to fixtures. No claim of whole-repository implicit-panic proof. |
| R02 | Pinned OTel resource/proto and existing D119 join authority; alternatives and live subject mismatch in the identity record. No exporter/ledger architecture selected by this correction. |
| R03 | Two actual required arguments and existing canonical64-hex identity bounds; no new production timeout/cap/cadence or benchmark-derived runtime number. Test bounds/bytes/ports are explicit caller inputs. |
| R04 | Existing tracked join relation, scope and typed collection outcomes; no new lifecycle state. Current wire cases exercise actual terminal/cause/credit transitions without claiming complete restore state. |
| R05 | Reuses held tracked provider, one Reader/codec and canonical observer. Rejects file walk/cache/second observer. No new polling or cache. Existing broader wait/cache obligations remain open. |
| R06 | Short provider selection/fallible two-ID copy; existing checked codec seam. Scoped source/fixture diff reviewed. |
| R07 | New comments explain canonical provider authority and protocol input; no code narration layer. |
| R08 | Current source9×1000 latency plus100-call allocation comparison; explicit compact4× miss. No whole-system performance win. |
| R09 | e37 wrong-harness failure distinguished from9be actual joiner mismatch before production fix. |
| R10 | Original failures, actual RED/green/mutation/restoration, hashes and harness mistakes retained/identified below. Overwritten initial reference receipts are explicitly not recovered. |
| R11 | All7/GPU/fmt and actual KVM amd64; native Mac remote corpus. Real HVF/WHP/ARM64 guests remain unverified; WinARM pause preserved. |
| R12 | Shared guest metrics/supervision, no host sensor/installed deliverable or duplicated OS implementation. |
| R13 | Existing private builtin now takes run+VM; own workload collection remains narrow. Production delivery and complete accounting stay open. |
| R14 | Source-specific latency/CPU/observed allocations/faults; current raw samples. RSS/copies/parent/kernel attribution remain open. |
| R15 | ROOT reviewed current scoped checkpoints, actual combined gates/mutation/consumers/profiles; final signed batch only after this review. Five lanes continue separately. |
| R16 | Exact generated lock preserved, all frozen binaries/evidence remain on owned runners, local Mac above30GiB. No active cache or user data deleted. |

## Durable raw records

[raw/verified.json](raw/verified.json) records coordinator verification of EVERY
unique safe relative regular member against original length/SHA256:

- Linux REDs/c138/combined:509 payloads+manifest,148146bytes,
  SHA256 `9e7feeb7cbc2a6284193e4fa8331f5d57f4dab49264a58fe86a5415c98730036`.
- Mac c138/0ae:28 payloads+manifest,10110bytes,
  SHA256 `eef25b105289b58b3e20f7c9f13daeb52190e4f66394c19d55c8bfd0bb4180b5`.
- Current consumers/profile/mutation/restoration:279 payloads+manifest,134918bytes,
  SHA256 `6c0316377f5631d68beb62e8cab755c005f63cfb59fc88646ba5b3a06c1b21e7`.

Archives preserve original retained bytes, not normalized summaries. Excluded
regenerable runtime caches/tmp, copied ELF binaries, guest/frozen build artifacts
and empty Docker config directories stay on the runner; artifact hashes and raw
compiler records identify them. Failed/successful logs, JSON, source diff/readback
and profile samples outside those directories are included. The original lost
reference receipt stdout is the explicit exception above. The current-controls
requested archive has172 payloads+1manifest, not173 payloads; ROOT verified its
original bytes separately. Ten ignored cases, full population/kernel return,
all-route mediation, effect ownership, native harness/credential contracts,
hard entitlements and management-generation restore/move remain open.
