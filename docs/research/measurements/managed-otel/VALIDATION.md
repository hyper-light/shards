# On-demand cgroup OTLP: coordinator evidence

Source: `28e147c4dda846ca023020e03bc9b00eb2e58e94`; production bytes equal
`3f7c4838b892cbf47e757ea802f5e1c9a3ff1426`. The later commit only flattens a
test diagnostic branch after an actual Clippy failure. These are narrow results,
not closure of G25 or P0–P13. Main remains `33df95b08` until coordinator adoption.

POLICY_READ: coordinator; current main `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS, rules, development-evidence, CLAUDE, management plan and handoff read.
Scope: init reader, existing builtin/owner CLI, source integration and evidence.
Research/codec/reader/reference implementers own scoped light checks; the
coordinator owns the VM, Collector, mutations, profiling and main merge.

## Actual caller and platform results

Fedora host: Linux `6.19.10-300.fc44.x86_64`, `/dev/kvm` usable. The normal
single-binary carried VM/network/init/server build and matching static testguest
passed. Test kernel SHA-256:
`48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7`.
Frozen CLI SHA-256:
`afdc30231f16c6d7d6955e005db15280918024872a79ab3c2e54cfbd2af820d1`.
Artifact paths/hashes are in the original `artifacts.json`.

| Check | Actual result |
| --- | --- |
| Current init metrics reader + encoder | 20 tests passed; native strict Clippy passed. Reader grammar fixtures are regular files, not invented cgroup authority. |
| Real `metrics::cgroup_metrics_use_declared_scopes_without_merging_member_output` | Ordinary KVM test: 1 passed, 0 failed/ignored, no skip. Mixed AGENT/HARNESS plus files-only member0 yields run/workload/member1/member2. Both tenants' fd1/fd2 bytes remain exact, main progresses, paused collection refuses with empty stdout, resume collects again; owner/observer cleanup returns FD7→7. |
| Existing `code_mode_runs_a_program_with_the_callers_grants` | Ordinary KVM test: 1 passed, 0 failed/ignored, no skip, after migration to the one canonical observer. Original effect/error/identity/cancellation assertions remain. |
| Supported seven-target `scripts/lint` | Exit0 including GPU and format. ARM64 Mac host, rustc1.99.1/LLVM23.1.1. This is compilation, not native WHP/HVF VM evidence. |
| Native Mac remote managed corpus | 15 passed, 0 failed/ignored. |
| Initial `3f7` gate | Reader20 passed, then strict Clippy failed the new test's nested-if style. Both Linux and Mac raw failures remain; neither is relabelled a full passing gate. |

The finite root runner admission was 4CPU/8GiB/16GiB. It is not a measured peak
or a hard physical cgroup claim. `linux-gate-28e.py` and `mac-gate-28e.py` retain
the exact calls, environment and cleanup bounds; the initial drivers remain too.

## Official decoding and actual HTTP acceptance

The [official decoder tool](../managed-otel-oracle/README.md) uses Collector
`pdata v1.71.0`, core revision `e883d737e49935984df6b064e512eed6cdc0d289`.
Fedora Go1.26.8 binary SHA-256:
`dc8281fbaa3f6e6b325958f706c31425d3f2f7783c639ebd2bce24c46dfee62b`.

The test-only official Collector core0.165.0 release archive was checked against
its published SHA-256. Binary SHA-256:
`14e254a6a0880d9b6f94487658e49fc9f3b0e14c62e82253be78da08e8c1ddd8`.
It is not installed or shipped. `collector-http.py` uses an OS-assigned loopback
listener, explicit bounded startup/request/cleanup, startup-log readiness, one
actual POST and graceful child wait. The owned listener witness excludes the
default metrics/grpc listeners. No telemetry sender retry/cache is introduced.

| Input | Decoder | Collector | Cleanup |
| --- | --- | --- | --- |
| Exact codec edge fixture, 4833B | 3 resources, 18 points; full u64 restore marker and i64 maximum preserved; overflowed memory absent | HTTP200, no rejected points/warning, all six metric names in decoded debug output | Collector exit0, log reader joined, controller FD4→4 |
| First live KVM export, 6492B | 4 resources, 24 points, exit0 | HTTP200, no rejection/warning | Collector exit0, controller FD4→4 |
| Second live KVM export after pause/resume, 6493B | 4 resources, 24 points, exit0 | HTTP200, no rejection/warning | Collector exit0, controller FD4→4 |

First JSON SHA-256: `27f66030260425597291930a1281a024924e207e51ddacc21d8935fb75528fbe`.
Second: `903cd33ecf205902613ab5362fa8797aaff0bb7b1d9bf9bcac5fb98ea83e5289`.
Exact requests, responses, decoded receipts, configs, listener witnesses and logs
are retained. A Collector acceptance is not proof of cgroup subject authority,
clock accuracy, counter continuity, exporter resource admission or sink durability.

## Live attribution mutation

`member-mutation-28e.py` changes only the emitted stable declared member ID:
`Scope::Member(domain.member)` → `Scope::Member(domain.member.saturating_sub(1))`.
This deliberately models renumbering runnable declarations over files-only member0.
The normal carried build succeeds, then the unchanged real KVM test fails101.
The actual OTLP artifact contains member0/member1 instead of member1/member2;
this is a false subject attribution, not just a missing diagnostic.

Restore the exact original `run.rs`, rebuild normally and rerun: ordinary test
passes with actual member1/member2. Both raw JSON artifacts, source-before,
exact one-line diff, original/restored source hashes and command results remain.
The test binary is unchanged across these two runs; the separate carried CLI/init
is rebuilt. The driver records test hashes, not a frozen mutant CLI hash; a claim
of independently retained mutant executable bytes would exceed this record.

## R01–R16 review and limits

| Rule | Applicable evidence / remaining obligation |
| --- | --- |
| R01 | New parser uses checked numeric arithmetic, fixed checked slices and fallible page/sample/argv reservations; typed errors, strict Clippy and boundary fixtures pass. Existing builtin `BufWriter::new` allocation and outer String error bridges remain source-observed audit limits, not fault-tested guarantees. No whole-path panic-freedom claim. |
| R02 | Pinned OTLP, semantic conventions, Rust/Collector alternatives and cgroup readers in the linked standards report; actual decoder/HTTP/VM evidence above. Encoding cost comparison is separate and pending. |
| R03 | D229 records SI conversion, protocol integer grammar, actual OS page, declared scope count and static diagnostic labels. No collection cadence, cache expiry, guessed counter origin or queue bound is added. Test deadlines/admissions are explicit fixture inputs, not runtime policy. |
| R04 | Typed scope/observation failure and parser states; unknown samples omitted with categorized diagnostics. VMM restore count is labelled separately from management generation. Restart/recreation and P11 remain open. |
| R05 | On-demand collection on existing owner/builtin transport; no periodic guest reader, registry cache or retained last-good sample. Paused collection refuses rather than emitting fresh cached data. Existing shared transport waits remain its responsibility. |
| R06 | Concrete fixed grammar and focused serializer; no new callback/state framework. Combined strict Clippy passes. |
| R07 | Comments retain kernel/protocol/unsafe obligations and non-obvious uncertainty. |
| R08 | 4–5× is a target, not a result. Equivalent encoding profiles pending; no full-SDK/VM speed claim. |
| R09 | Baseline command absence reproduced on frozen main-equivalent CLI; real new feature and false-attribution mutation above. Source observations about existing defaulting/allocation are not reported as reproduced defects. |
| R10 | Original commands/outputs/source hashes retained compressed without whitespace normalization, transferred and every original verified. Remaining gaps are explicit. |
| R11 | Supported seven-target compile/lint; real KVM amd64 only for this slice. Native HVF, WHP and arm64 guest coverage remain unverified. Paused Windows ARM VM stays untouched. |
| R12 | Linux guest implementation is shared across host platforms through existing supervision/CLI; no host sensor or installed second deliverable. |
| R13 | One on-demand owner command and one reader/codec. This produces an OTLP request artifact; production HTTP delivery/typed partial-success/retry is not implemented. |
| R14 | Allocation/CPU/fault/latency encoding profiles pending. Parent/kernel producer accounting and protected control capacity are not inferred from child counters or FD return. |
| R15 | Scoped signed implementation checkpoints, coordinator gates/mutation/Collector and review. Five assigned private lanes remain separate; completed test cases do not close the management plan. |
| R16 | Sources/evidence/frozen binaries preserved; root Mac remains above30GiB. Only regenerable, inactive output is eligible for cleanup. |

Run/workload/member cgroup observations overlap; do not sum them as disjoint VM
usage. `pids.current` counts TIDs, not only process leaders. Host-backed share
I/O and physical VM/kernel costs outside these groups are not included. Sequential
reads are observations, not atomic reservations. Missing birth time weakens reset
interpretation; cgroup recreation and management generation must still be solved.

Further required work includes whole-system/process/manager-owned observations,
true control and parent/kernel accounting, source-specific fault/restore tests,
production delivery, supported real hosts and the rest of P0–P13.

## Raw storage

[verified.json](raw-storage/verified.json) names the exact original archives and
verification results. Linux originals: 188 files in `linux-originals.tar.gz`,
SHA-256 `7eb46921f55aed18ac82d7a268f3cf5f95990b2ada8b6fa08bdbb2f83b5239f3`.
Mac originals: 25 files, `mac-originals.tar.gz`, SHA-256
`5899787943f56c79a81e9cae932db3109fd38ec487d90f257bb3e004248f3ca8`.
Mutation/restoration: 31 files, `member-mutation-originals.tar.gz`, SHA-256
`620ca0f0f1031c4eb72d2f2b642c97450dbf3e6c3060c2cc23dadeed7638259b`.
Each archive includes a manifest of original sizes/hashes; all members are unique
safe relative regular files. Frozen binaries/build caches are excluded and remain
on their owned runner. The archive manifest explicitly lists excluded paths;
no matching log/JSON/script/text evidence outside those caches was omitted.
