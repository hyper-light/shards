# OTLP contract for local cgroup memory and task ceilings

Research recommendation for ROOT, 2026-11-10. **No export contract is adopted by
this note.** The typed readers at `c5caa0c99d0bbf1213a0953598e55014c512f04e`
remain unexported; ROOT owns caller wiring, selection, whole-caller proof and
main merge. No Reader/Snapshot/codec/run/daemon edit, build, VM, Collector,
measurement or profile was performed for this research.

`POLICY_READ`: current fetched origin/main
`b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe`, AGENTS.md, FULL R01–R16 and
development-evidence, current managed plan and `/Users/adalundhe/g121-handoff.md`.
Rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
CLAUDE is byte-identical to the preceding full read. The handoff still names
older a920; the current user instruction and b57 policy govern. Role: implementer,
research only, `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Owned path: **this file only**, including
primary-source identities below. Prior signed source/raw/targets are preserved.

## Pinned contracts and observed implementations

Pins reuse the [existing OTel report](managed-runtime-otel-metrics.md), adding
resolved release pins for the systemd comparisons. Release tags were resolved
through official GitHub APIs; annotated tags were peeled to commits.

| Source | Release / exact commit |
| --- | --- |
| OTel specification | 1.62.0 / `18c5c1706e346fec3bdb5e135a6e3a78f461e586` |
| OTLP protobuf | 1.11.1 / `b3f75588eb23c5fca62264edd05d382de49beb1a` |
| Semantic conventions | 1.45.0 / `fb30ca17193b551473b1f43c315482918ab6987c` |
| Collector contrib | 0.165.1 / `6083b2c53d78b21d0925b703194aaed920c01af5` |
| cAdvisor | 0.56.2 / `7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0` |
| opencontainers/cgroups, also cAdvisor's actual go.mod dependency | 0.0.6 / `e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b` |
| systemd | 258 / `781d9d0789379d1ea1f2ecefb804d41e9c8b6c38` |
| prometheus-community/systemd_exporter | 0.7.0 / `8b490c0159f2d3b80d1721c1e632a021f496f375` |
| prometheus/node_exporter | 1.12.1 / `6044da783597cc3b57aef7580ddcdcff58a4ee99` |
| Linux contract | 6.18.48 / `5bbb9c9f8f808710e2123f2b30f0d61d7d698f52` |
| Official ProtoJSON guide source, resolved at retrieval | `51aa6af0521be1cc618586caa376b99ddb480247` |

These independent version axes are not a new dependency/toolchain selection.
Source inspection establishes behavior in these revisions, not live acceptance,
correctness of every upstream path or performance.

## Preserve what was actually read

| Quantity | Meaning and boundary |
| --- | --- |
| Local `memory.max` | Actual kernel-file configured ceiling in bytes, `Finite(u64)` or exact symbolic `max`. Preserve the read-back value, including kernel page rounding, rather than the owner's pre-write request. Local `max` means no limit at this node, not infinite physical memory. |
| Local `pids.max` | Configured task/TID ceiling, including threads. It is not a process-leader count or a guaranteed number of new tasks. Hierarchical charge checks apply; lowering a cap or migrating tasks can leave `pids.current > pids.max`. |
| Effective ancestor ceiling | A separately defined result over a complete authoritative ancestor path. A local finite ceiling may be looser; a local `max` may have a finite ancestor. Missing ancestors make the result unknown. A minimum of configured ceilings is still not free capacity. |
| Availability/headroom | Requires additional contemporaneous usage, competing sibling use, reclaim and global limits. Subtracting local usage from a local or ancestor cap does not reserve the result or prove allocation/fork admission. Reads here are non-atomic. |
| Entitlement/protection | Requires an explicit checked owner contract and enforcement/charging policy. Neither a cap nor `memory.min`/`memory.low` alone creates the missing Shards owner admission producer. D59/P0 remains ROOT's decision. |

Basis: pinned [kernel interface/model][kernel-cgroups] and [PID implementation][kernel-pids].
The [Shards managed plan](../design/managed-runtime-plan.md#5-resources-delegation-and-control-survival)
keeps authoritative admission separate. No `max`-to-MemTotal substitution, parent
walk, remaining-budget calculation or stronger owner promise is selected here.

Systemd is an instructive **different** subject: `MemoryMax` and `TasksMax` are
configured unit properties; `EffectiveMemoryMax` / `EffectiveTasksMax` minimize
configured unit/slice limits and root physical-memory/system-task bounds.
The actual [`unit_get_effective_limit` implementation][systemd-cgroup] reads
systemd context, not all kernel held-file readbacks. Its [D-Bus getter][systemd-dbus-unit]
and [resource-control manual][systemd-resource-control] do not turn that result
into available/reserved resources. Do not label the Shards local Reader as this
effective systemd property.

## OTel wire and semantic constraints

OTLP Gauge is a sampled, non-additive scalar; it has no aggregation temporality
and ignores start time. A local ceiling is not summable across overlapping
run/workload/member ancestors or independent policies. Gauge is therefore a
better raw-ceiling contract than an UpDownCounter/Sum. This is a recommendation
from the [data model][otel-model] and [wire schema][otlp-metrics], not a claim
that upstream exporters all choose it.

`NumberDataPoint.value` is **only signed 64-bit integer or double**. No string,
unsigned-integer, null or finite/max union exists. JSON `asInt` uses a decimal
string because of ProtoJSON encoding; that spelling does not enlarge int64.
Use checked conversion for a nonnegative exact integer range
`0..=9_223_372_036_854_775_807`. Reader values above it are finite but not exactly
representable by that integer contract: omit the numeric point and report typed
encoding overflow; never wrap, clamp or reinterpret the value as unbounded.
Binary64 can carry the u64 magnitude but not every integer above `2^53` exactly.
Uniform double or mixed integer/double streams are wire alternatives, not an
exact-full-u64 solution. [Schema][otlp-metrics], [OTLP JSON][otlp-protocol],
[ProtoJSON integer/float rules][protojson].

The inspected 1.45.0 [container table][semconv-container] does not define a raw
local `memory.max` / `pids.max` pair. Its `container.memory.available` is
limit-minus-working-set and is omitted when its limit is undefined. The
[system table][semconv-system] defines host-subject `system.memory.limit` as an
UpDownCounter in bytes; that is not this cgroup observation. These inspected
tables are not a claim about every possible vendor metric. Names in Collector
metadata are not automatically semantic conventions. Use custom Shards names
for the actual subject; units follow [the general conventions][semconv-general].

Resource identifies the observed entity; its identifying attributes do not
change over that entity's lifetime. Keep the existing run, microVM, VMM restore
generation, cgroup scope and stable member identity. Do not put a mutable cap,
`max`, current state or errno into Resource attributes, or create a new resource
on each cap change. Metric point attributes may carry a **bounded typed state**.
[Resource model][otel-resource]. VMM restore count remains distinct from a
management epoch, as already documented; this note changes neither.

## What established exporters actually do

| Pinned implementation and actual path | Finite / unbounded / errors; meaning for this decision |
| --- | --- |
| Collector contrib Docker stats [metadata][collector-docker-metadata] + [`recordMemoryMetrics` / `recordPidsMetrics`][collector-docker-receiver] | `container.memory.usage.limit`, `By`, cumulative nonmonotonic int Sum, defaults enabled; direct `int64(memoryStats.Limit)`. `container.pids.limit`, `{pids}`, same Sum shape, defaults disabled; emitted only when API current and limit are both nonzero. These receive numeric Docker API facts, with no symbolic-max branch here. No dedicated unbounded state or checked u64-to-i64 conversion appears at these call sites. This is not evidence that raw finite zero or unavailable kernel reads should be defaulted. Upstream API normalization outside these call sites is not inferred. |
| cAdvisor [memory/PID spec reader][cadvisor-common], [Prometheus exporter][cadvisor-prometheus], [thread mapping][cadvisor-handler] | `container_spec_memory_limit_bytes` is a float64 Gauge. Its `specMemoryValue` converts values above its own `2^62` threshold to zero. The spec reader maps `max` to MaxUint64 and empty/parse-error text to zero. `container_threads_max` is a Gauge whose help explicitly gives zero an infinity meaning; `ThreadsMax` comes from the cgroups PID stats. These are historical producer conventions, not truthful raw Shards zero/unbounded/error distinctions. The raw [root handler][cadvisor-raw] additionally uses machine memory for the root-group case. |
| cAdvisor's actual [cgroups dependency][cadvisor-go-mod], [PID reader][cgroups-pids], [scalar helper][cgroups-fscommon] | Helper maps `max` to MaxUint64; PID stats remap it to historical zero. Controller-missing fallback also uses zero for its PID limit. The [memory reader][cgroups-memory] retains the helper's numeric max. This explains why copying an established library's numeric fields would lose the Reader's tagged distinction. No upstream library is adopted. |
| node_exporter [`collectUnitTasksMetrics`][node-systemd] | `node_systemd_unit_tasks_max`, float64 Gauge: reads systemd `TasksMax`, omits MaxUint64 and property-read failures; finite values are cast to float64. Omission is established practice but does not independently distinguish unbounded from failed collection. |
| systemd_exporter [`collectServiceTasksMetrics`][systemd-exporter] | `systemd_unit_tasks_max`, float64 Gauge: likewise omits MaxUint64, returns property/conversion errors to its collector, and exports finite numeric values. The inspected task path uses `TasksMax`, not `EffectiveTasksMax`; no memory-max exporter is claimed from this task function. |

Upstream zero/sentinel/threshold behavior above is a **source observation**,
not a reproduced defect or permission to transplant those constants. It supplies
comparison evidence, not a Shards guarantee. Node_exporter's
[`unit_state` loop][node-systemd] separately demonstrates an established bounded
state-label Gauge pattern: every known state is emitted as a true/false indicator.

## Representation options considered before recommending

| Option | Range, unbounded and failure outcome | Assessment |
| --- | --- | --- |
| Exact custom integer Gauge + separate typed state/diagnostics | Finite `0..=i64::MAX`; no numeric point on max/error/out-of-range; distinguish each in state/diagnostics. | Recommended semantic core; preserves exact zero and reason. State carrier and transition handling still need ROOT selection/proof. |
| Finite Gauge with omission alone | Same numeric range; max, absent, unreadable and overflow all disappear identically. | Insufficient to meet the requested typed distinction; a prior finite value may remain in a downstream lookback window. |
| Double Gauge, with `+Infinity` for max | Binary64 magnitude, integer precision loss possible; ProtoJSON permits the special `"Infinity"` spelling. | Wire-representable alternative, not a pinned cgroup semantic convention. Does not encode read failure separately; actual serializer/Collector/backend behavior is unrun. Prefer explicit state over a special floating value. |
| Ordinary NaN, null or value-less point | NaN is a float value, not the typed cause; null unsets the oneof, which ordinary points require. | Does not implement the intended finite/max/error union. Do not substitute NaN for a specified missing marker. |
| Standard `NO_RECORDED_VALUE` flag | Schema mask1 marks explicit missing/stale data, not success-unbounded or a reason. | Useful **additional** finite-series removal mechanism. Model says other payload properties should be ignored; schema also requires the number oneof. ROOT must prove a chosen valid flagged encoding and downstream removal before adoption; it cannot replace separate state. |
| Numeric sentinel zero, -1, MaxInt64/MaxUint64 or upstream threshold | Collides with finite zero/legitimate values, is outside the exact wire range, or imports an unrelated producer convention. | Reject for this raw contract. Zero is emitted as a cap only after an actual `Finite(0)` result. |
| Decimal/string `max` in the metric value or cap-valued point attributes | String is outside NumberDataPoint; putting every finite value in attributes creates value-dependent series and removes numeric semantics. | Reject as a numeric metric. Rich raw evidence could carry a decimal string separately, under its own bounded contract. |
| Mutable resource attributes for value/state | AnyValue supports strings, but cap changes alter Resource identity. | Reject resource churn; keep state on bounded points or existing typed diagnostic channels. |
| Replace local max with host/effective/available value; adopt an unfitting standard name | Changes subject/meaning and may depend on incomplete ancestor state. | Separate future instruments only after their actual data/authority exists. No current default or entitlement inference. |

The missing-marker distinction follows [model flags][otel-model] and [schema flags/oneof][otlp-metrics].
No flag encoding, infinity handling or backend lookback behavior was tested here.
Omission must not be described as proven deletion of a previously exported value.

## Evidence-supported recommendation for ROOT's decision

Retain the earlier candidate `shards.cgroup.memory.limit`, Gauge `By`, and add
`shards.cgroup.task.limit`, Gauge `{task}`. Descriptions must say **local configured
ceiling read from memory.max/pids.max**, with ancestor/availability/entitlement
exclusions. Both use the exact nonnegative int64 range above; do not change
numeric type at an overflow boundary to conceal uncertainty. These are custom
names, not instruments standardized by semantic schema1.45.0.

Prefer a separate `shards.cgroup.limit.state`, Gauge `1`, with fixed point
attributes `shards.cgroup.metric` (one of those two names) and
`shards.cgroup.limit.state` (`finite`, `unbounded`, `unavailable`). Emit all three
logical indicators for a requested/attempted field at the sample time, derived
from the **same typed result**: one true1, the others false0. This finite enum
derives the bounded state dimensions; 0/1 are truth values, never numeric cap
defaults. Existing [Shards diagnostics at b57](https://github.com/hyper-light/shards/blob/b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe/crates/init/src/metrics/otlp.rs#L190)
already provide `shards.cgroup.collection.error.count` and per-metric
`shards.cgroup.collection.error`, with typed missing/invalid/overflow/io and errno.
Reuse that distinction rather than classifying successful max as an error.

| Reader/encoding result | Numeric limit Gauge | State indicator | Existing error diagnostics |
| --- | --- | --- | --- |
| `Finite(v)` within signed range, including0 | Exact v | finite1, other states0 | No limit error. |
| `Unbounded` | Omitted | unbounded1, others0 | No error merely because the file says max. |
| Existing `Failure` (empty/missing/invalid/overflow/io) | Omitted | unavailable1, others0 | Preserve actual typed reason; missing file is Io(ENOENT), not symbolic max. |
| `Finite(v)` above signed range | Omitted | finite1, others0 | Typed encoding overflow; the read still established a finite cap. |
| No requested/attempted observation | No invented point | No invented observed state | Do not fabricate a failed read, unbounded state or successful zero. |

This is the recommended contract after the comparisons, **not authorized code
or ROOT's final decision**. A boolean unbounded companion plus existing errors
is a smaller alternative, but omission on failure needs equally explicit freshness
rules. A single active-state-only indicator is also smaller; emitting every
state avoids leaving an old true indicator when state changes. ROOT must select
and account for the actual extra points/bytes; no overhead allowance is invented.
Keep numeric/state/diagnostic observations tied to the original scope and sample;
consumers must not use a historical finite point as current when fresh state
says unbounded/unavailable. Controller reads remain non-atomic across fields.

ROOT still owes exact fixture and real-caller/Collector/decoder checks for
finite0, signed max, above-range finite, symbolic max, absent controller,
malformed/unreadable sources and finite→max→error→finite transitions; matching
run/member identity, stable Resource identity under cap changes, units/Gauge
type/time and diagnostic causes; supported GNU/musl/architecture targets and
real controller readback/cleanup. Any selected stale marker needs actual exporter
and query/removal proof. Prior normal-export or Reader-only passes do not prove
this future contract. ROOT alone measures cost and adopts/merges.

## Research method, boundaries and R01–R16

Read-only primary downloads were made with Python3.9 stdlib
`urllib.request.urlopen(url, timeout=30).read()`, recorded as byte length plus
`hashlib.sha256(data).hexdigest()`. Each immutable raw URL is linked in the
identity table below. Local `rg -n` and line-range reads inspected the named
functions; no exporter executable was run. Original downloads and discovery
output remain in `/tmp/managed-limit-contract-b57-primary-v1`, about1.2MB of
sources, separate from all build/root caches. This is research evidence storage,
not a runtime cache or a repository clone.

Initial API directory discovery using systemd_exporter ref `master` returned404;
its pinned release root resolved successfully, and a guessed `collector/`
directory also returned404 before the real `systemd/systemd.go` was read. The web
viewer could not fetch ProtoJSON's GitHub source link; the official repository
API resolved its exact commit and raw source successfully. These are retained
research-discovery failures, not compile/test results or production defects.
No test, profile, live controller or Collector result is claimed by this note.

| Rules | Evidence / unrun boundary |
| --- | --- |
| R01/R04/R06/R07/R13 | One research file only; explicit finite/unbounded/failure/range semantics and concise existing-seam recommendation. No production path or abstraction introduced. |
| R02/R03/R05/R12 | Exact official model/schema/semconv/kernel and actual established implementation paths compared first. Numeric limits derive from wire widths; upstream sentinels/thresholds are observations, not new policy. No waits/cache/topology/host adapter selected. |
| R08/R09/R10/R14 | No superiority, live-defect, performance or completion claim. Pins/download identities/method and discovery failures retained; ROOT's actual conformance/transition/mutation/profiling work remains unrun. |
| R11 | No new platform result inferred; shared guest Reader remains pending caller wiring, full GNU/musl/architectures and VM matrix owed ROOT. macOS ARM64 only; paused WinARM untouched. |
| R15/R16 | Implementer/research only; no children/goals/builds/VM/audit/campaign/cache deletion/root writes/main merge. Same own branch, named-path signed gits return, required attribution. Original source/binaries/raw evidence preserved. |

Actual `python3 scripts/check-development-policy` returned0 with
`Development policy checks passed; evidence/coordinator review is still required.`
The named new-document whitespace check is recorded with the signed return.
These mechanical results do not establish research correctness, future wire
acceptance or implementation readiness. Full P0–P13/G25 and parent guarantees remain open.

The first local reference-check expression exited1 because it counted display
labels in full Markdown references as unresolved reference IDs. That checker
failure did not identify a broken source link. The corrected check distinguishes
full and shortcut references; its result accompanies the signed return. No
production change or source conclusion was derived from the failed expression.

## Exact downloaded primary-source identities

The following records identify bytes actually downloaded. Immutable URLs pin
the file revision; downloads used `https://raw.githubusercontent.com/OWNER/REPO/COMMIT/PATH`
for each linked GitHub blob. SHA256 detects a different response. No secondary source is
used as authority. Files downloaded for context but not selected as a contract
remain identified as well. Source links above resolve to this table's exact pins.

| File key / immutable downloaded source | Bytes | SHA256 |
| --- | ---: | --- |
| [cadvisor-common] | 12896 | `60f187a097a92ec82c5f66b32fa8904315903d99d5235847eada1d34bd1f2fa0` |
| [cadvisor-go-mod] | 3561 | `ba47175a9b3f30ef3292f97e03ab42bc45d318b29f28d05e482e56a94c0a8f8b` |
| [cadvisor-handler] | 28265 | `0cad9021bf0feb64a5b1d044c1a0e901b96054668f7ee7c17eabecf44da30007` |
| [cadvisor-helpers] | 4842 | `81308abdb835d257f147d4e64ceb10c170286829dc01fef6882a07c9c6c80b54` |
| [cadvisor-prometheus] | 78137 | `83aa4f737291512eff101918d5b8e3911183fe5c58a12272a392e3058697d211` |
| [cadvisor-raw] | 8647 | `53004c3f091b7a2e22528e2ed11da669fc797ca7ec5521bd8c097ace45fb0255` |
| [cgroups-fscommon] | 3812 | `8cff593ec208255c41b786dba2b82f022c1fe997f8b69b0b4756e62a4e7c7ccc` |
| [cgroups-memory] | 6453 | `5307e4bf4d14d8d6a50a30f537208d80a2515d04cac1bc5e4872e80056a00699` |
| [cgroups-pids] | 2018 | `94866ec216cda4bbb65026646e09efa87d2a75ca8ceac33c384d01d7d465e339` |
| [cgroups-read] | 12988 | `1b378a5d3dc2285542cebe39187c96cda39d65795df21d7ef5553725bfb4ee79` |
| [collector-docker-helper] | 2286 | `12c1deb0f393dd0e063f51e0b2fd992693221ff75f10cc34583c3c4bcd0aa783` |
| [collector-docker-metadata] | 28002 | `9d51b2f483dd82650ff6f30636a05067c82a9489c0b90e7c33561162b590e87b` |
| [collector-docker-other] | 2873 | `908f8a6889b05bdb0ef4040174fd43bb7fcfec3f68b79640e04f62a683ab35af` |
| [collector-docker-receiver] | 16340 | `10442b2a6bba0cf0824585b6927c6777b05ad8f596efd97c92b957014354d091` |
| [kernel-cgroups] | 128518 | `e81d1d9be76afdc6ee0a28f109353b1fd0a1f94df0397652988eac0bce4b87cd` |
| [kernel-pids] | 11827 | `802a4643e30c5cedd8bd67f5dec91ee28bec802cec9b1169a448ee2f233fd634` |
| [node-systemd] | 19968 | `73659c919e892409fdcf8b38f2fd7d0bc98c1eb870539673d42a617239836ce9` |
| [otel-api] | 55771 | `a0b39f47f810aaafc8883ad326526a376c54e6a3ef8fdb8e28f46c4c9513d670` |
| [otel-model] | 62383 | `33fc0b88a7574501db5bda1c52624d9a0095e258edce39590bef9b1dd04fb1ea` |
| [otel-resource] | 7370 | `063c0ff48ed92bafbbfd2a16046b5481a05559a483d5f0270c7b87241f8ff6e0` |
| [otlp-metrics] | 31935 | `9663b2670f980041e73a71a7c09fdf4fe0bc64056be762f2fcbd2642c4d112a3` |
| [otlp-protocol] | 39546 | `18525fa29803740421409d87a07d56cbaa47c204405811618463c823cb770486` |
| [protojson] | 31368 | `2b2afde43c340de21ebe41fff76a1a67e2d2984314616b24afef65c56003530f` |
| [semconv-container] | 26589 | `97fb8880711a5a38bcce463b422cc6f712122dce0c92bd4faf7232a078320a6e` |
| [semconv-general] | 7043 | `948fe831ec6fcc153d8fcf089477e7beefb968ac7beafe100610fda7e93d7c26` |
| [semconv-system] | 83406 | `70497b832dbbc5cee8f347d3f2c7c287591464f34f39883fe2bcfffa35f286ac` |
| [systemd-cgroup] | 175946 | `a7822f3a45e813b8b333e1f1f6499d1b556a5d3a9a7ab26e5a456bd6fa5460da` |
| [systemd-dbus-cgroup] | 87861 | `617f4c9d9b64da0b86c5a5ce2accdd3d7af2603200810c17cffc85e6408e3cdd` |
| [systemd-dbus-unit] | 109684 | `6eb29f778117eae2e931b53d90cbfe50205eb9ff789fbc0f182bd253f79218a1` |
| [systemd-exporter] | 28937 | `51443370cd8e37973a7607fa9c6f930af342fb32f91cd65afd213957b5a06b2c` |
| [systemd-resource-control] | 99883 | `6899aad25cc334478e2f5e13cf1abc137e30a93db9f38974a7317799f347319b` |

The original failed discovery log is retained at the private source directory's
`discovery-original.log`: 3229 bytes, SHA256
`4c468e487f6fbf8d676f1e28511b77f3211382d7978a3b629a1b89d3140e158f`. Its command exited1 during
directory discovery after the successful first primary downloads; it was not
a production validation run.

Inspected Shards `crates/init/src/metrics.rs`: 10207 bytes, SHA256
`f023a1aaa04597c3a9bf4094557a6e6545b670769421f12e27571ff2450673f9`; immutable revisions are named
above. This document does not alter those bytes.

Inspected Shards `crates/init/src/metrics/otlp.rs`: 17125 bytes, SHA256
`30fb309612e76d000b9d582829ed535e8f1a6cab569df10d36bbbce6e9c0d339`; immutable revisions are named
above. This document does not alter those bytes.

[cadvisor-common]: https://github.com/google/cadvisor/blob/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0/container/common/helpers.go
[cadvisor-go-mod]: https://github.com/google/cadvisor/blob/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0/go.mod
[cadvisor-handler]: https://github.com/google/cadvisor/blob/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0/container/libcontainer/handler.go
[cadvisor-helpers]: https://github.com/google/cadvisor/blob/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0/container/libcontainer/helpers.go
[cadvisor-prometheus]: https://github.com/google/cadvisor/blob/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0/metrics/prometheus.go
[cadvisor-raw]: https://github.com/google/cadvisor/blob/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0/container/raw/handler.go
[cgroups-fscommon]: https://github.com/opencontainers/cgroups/blob/e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b/fscommon/utils.go
[cgroups-memory]: https://github.com/opencontainers/cgroups/blob/e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b/fs2/memory.go
[cgroups-pids]: https://github.com/opencontainers/cgroups/blob/e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b/fs2/pids.go
[cgroups-read]: https://github.com/opencontainers/cgroups/blob/e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b/utils.go
[collector-docker-helper]: https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/6083b2c53d78b21d0925b703194aaed920c01af5/receiver/dockerstatsreceiver/metric_helper.go
[collector-docker-metadata]: https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/6083b2c53d78b21d0925b703194aaed920c01af5/receiver/dockerstatsreceiver/metadata.yaml
[collector-docker-other]: https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/6083b2c53d78b21d0925b703194aaed920c01af5/receiver/dockerstatsreceiver/metric_helper_others.go
[collector-docker-receiver]: https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/6083b2c53d78b21d0925b703194aaed920c01af5/receiver/dockerstatsreceiver/receiver.go
[kernel-cgroups]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/Documentation/admin-guide/cgroup-v2.rst
[kernel-pids]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/kernel/cgroup/pids.c
[node-systemd]: https://github.com/prometheus/node_exporter/blob/6044da783597cc3b57aef7580ddcdcff58a4ee99/collector/systemd_linux.go
[otel-api]: https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/metrics/api.md
[otel-model]: https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/metrics/data-model.md
[otel-resource]: https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/resource/data-model.md
[otlp-metrics]: https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/metrics/v1/metrics.proto
[otlp-protocol]: https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/docs/specification.md
[protojson]: https://github.com/protocolbuffers/protocolbuffers.github.io/blob/51aa6af0521be1cc618586caa376b99ddb480247/content/programming-guides/json.md
[semconv-container]: https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/system/container-metrics.md
[semconv-general]: https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/general/metrics.md
[semconv-system]: https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/system/system-metrics.md
[systemd-cgroup]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/core/cgroup.c
[systemd-dbus-cgroup]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/core/dbus-cgroup.c
[systemd-dbus-unit]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/src/core/dbus-unit.c
[systemd-exporter]: https://github.com/prometheus-community/systemd_exporter/blob/8b490c0159f2d3b80d1721c1e632a021f496f375/systemd/systemd.go
[systemd-resource-control]: https://github.com/systemd/systemd/blob/781d9d0789379d1ea1f2ecefb804d41e9c8b6c38/man/systemd.resource-control.xml
