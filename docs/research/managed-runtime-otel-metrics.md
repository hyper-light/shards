# OpenTelemetry metrics for managed Shards microVMs

Research checkpoint, 2026-11-10, against main
`33df95b08ad8f95bddeaa1c48acf0c64386e0eac`. Role: delegated implementer,
research only. The coordinator selects architecture and owns real Collector
conformance and profiling. No public command, topology, dependency, sampling
schedule or production constant is selected here. No production code, build,
VM, Collector or benchmark was run by this lane.

`POLICY_READ`: AGENTS.md, full R01–R16, development-evidence.md, CLAUDE.md,
managed-runtime-plan.md and `/Users/adalundhe/g121-handoff.md`; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Owned path: this file only. Paused ARM64 Windows and rejected campaigns remain
held. This checkpoint does not close P9 or the management plan.

## 1. Exact source pins

Official latest-release APIs were queried and tags resolved to commits before
reading source. These independent version axes must not be conflated.

| Contract | Inspected release | Exact commit |
| --- | --- | --- |
| OTel API/SDK and metrics model | [v1.62.0](https://github.com/open-telemetry/opentelemetry-specification/releases/tag/v1.62.0) | `18c5c1706e346fec3bdb5e135a6e3a78f461e586` |
| OTLP and protobuf schemas | [v1.11.1](https://github.com/open-telemetry/opentelemetry-proto/releases/tag/v1.11.1) | `b3f75588eb23c5fca62264edd05d382de49beb1a` |
| Semantic conventions | [v1.45.0](https://github.com/open-telemetry/semantic-conventions/releases/tag/v1.45.0) | `fb30ca17193b551473b1f43c315482918ab6987c` |
| Rust API/SDK/OTLP/HTTP/generated-proto crates | [0.33.1](https://github.com/open-telemetry/opentelemetry-rust/releases/tag/opentelemetry-0.33.1) | `372ebbc74d4dba71bb9820732f57f7316b4e7a1e` |
| Collector contrib comparison | [v0.165.1](https://github.com/open-telemetry/opentelemetry-collector-contrib/releases/tag/v0.165.1) | `6083b2c53d78b21d0925b703194aaed920c01af5` |
| Linux cgroup/proc contract | [v6.18.48](https://github.com/gregkh/linux/tree/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52) | `5bbb9c9f8f808710e2123f2b30f0d61d7d698f52` |
| Established cgroup reader | [opencontainers/cgroups v0.0.6](https://github.com/opencontainers/cgroups/tree/e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b) | `e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b` |
| Established resource reader | [cAdvisor v0.56.2](https://github.com/google/cadvisor/tree/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0) | `7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0` |

Semantic schema URL: `https://opentelemetry.io/schemas/1.45.0`; schema-file
format: 1.1.0. System instruments are **Development**; many process/container
instruments are **Release Candidate**, others Development. Check each instrument
and attribute rather than interpreting a page heading as stability. The schema
contains actual renames; a v1 service namespace does not freeze semantic names.
[schema](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/schemas/1.45.0)

Rust 0.33.1's proto submodule is
`ca839c51f706f5d53bfb46f06c3e90c3af3a52c6` (v1.10 preparation). Direct byte
comparison found its metrics-service schema identical to v1.11.1, but
metrics.proto differs. A crate release does not imply the newest schema.
[generation inputs](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-proto/tests/grpc_build.rs),
[embedded revision](https://github.com/open-telemetry/opentelemetry-proto/tree/ca839c51f706f5d53bfb46f06c3e90c3af3a52c6)

Bounded Shards inventory: `.rs`, Cargo manifests/lock and Markdown under
crates/docs, whole-word case-insensitive `opentelemetry|otlp|otel`, excluding
testdata, vendor and measurement artifacts. Matches were existing research;
no production OTel dependency/named implementation was found. This does not mean
Shards has no existing measurements.

## 2. Preserve the measured subject

Resources describe the observed entity, not necessarily the exporter process.
A host can be a VM. A host daemon exporting guest-system facts must therefore
not stamp the physical machine's identity onto those facts. InstrumentationScope
identifies instrumentation/version, not the tenant.
[resource model](https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/resource/data-model.md),
[host entity](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/resource/host.md)

| Actual subject | Identity and mapping constraint |
| --- | --- |
| Guest kernel/whole microVM | Provider-owned VM identity can satisfy `host.id`; actual `host.arch`/guest OS. A copied hostname/image digest is not unique. Keep Shards run/execution generation separately. `system.*` requires whole-system facts. |
| Run workload cgroup | Trusted run/generation and explicit aggregate-group scope. Includes descendants and possibly many managed members; it is not all guest/kernel consumption. |
| AGENT/HARNESS member cgroup | Stable declared member plus run/execution generation. Contains leader, descendants and threads; do not describe it as one process. Files-only and runnable declarations retain distinct stable identities. |
| Actual OS process | Known PID namespace, PID and creation identity, containing VM/run/member. Reused PID and harness-reported tool IDs do not identify a lifetime. |
| Host VMM/share/network/helper process | Physical-host identity plus that native process's identity. Native process usage is host cost, not guest use; shared producer attribution requires actual accounting. |
| Broker execution/stream/flow | Broker-owned identity internally; aggregate metric dimensions by true outcome/channel/route class. Per-call IDs belong to richer correlated evidence where permitted. |

Current process entity requires `process.pid` and ISO-8601
`process.creation.time`. Its `process.linux.cgroup` attribute reports membership,
not a new group-as-process definition. Service attributes describe a real service
instance, not every declaration or exporter convenience.
[process resource](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/resource/process.md),
[service resource](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/resource/service.md)

A cgroup, OCI image or Docker-compatible CLI alone does not establish a container
entity's runtime/API semantics. Do not invent Docker, Kubernetes or cAdvisor
identity/provenance. If a Shards workload explicitly meets a documented container
contract, evaluate relevant container conventions individually; otherwise use
the project namespace. Custom names below are candidates, not a chosen schema.
[container entity](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/registry/entities/container.md),
[naming](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/general/naming.md)

ResourceMetrics.schema_url and ScopeMetrics.schema_url cover different levels;
the latter covers metric names/descriptions/attributes. The OTel schema URL
does not standardize custom Shards extensions. Version that extension separately
and document which standard mappings use 1.45.0.
[wire hierarchy](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/metrics/v1/metrics.proto)

## 3. Raw counters and truthful metric mappings

Pinned [guest `stats`](../../crates/init/src/run.rs) returns one selected
cgroup's text plus guest `/proc` totals. [Host `Usage`](../../crates/shards/src/daemon/commands.rs)
derives display percentages/working memory and retains LAST_USAGE for paused VMs.
Several missing/invalid fields currently become zero. The wire lacks per-field
availability/start/sample time. An encoder cannot recover those facts after
defaulting. This source constraint calls for a typed collection seam; no defect
was reproduced or fixed in this research.

| Source and actual scope | Candidate instrument → wire type; unit | Interpretation |
| --- | --- | --- |
| `cpu.stat:usage_usec`, group/subtree | `shards.cgroup.cpu.time`, observable Counter → cumulative monotonic Sum; `s` | Divide by 1,000,000 as a unit conversion. Observe absolute kernel value; do not add it repeatedly to a synchronous Counter. Optional user/system modes must not coexist with a duplicate summable total. |
| `memory.current`, group/subtree | `shards.cgroup.memory.usage`, observable UpDownCounter → cumulative non-monotonic Sum; `By` | Current charged memory, not process RSS/PSS or whole-VM usage. Parent/child observations overlap and must not be summed as disjoint consumption. |
| `memory.stat:inactive_file` | `shards.cgroup.memory.inactive_file`, observable UpDownCounter; `By` | Raw reported category. A derived working-set formula needs its own name and underflow/availability handling. |
| Finite `memory.max` | `shards.cgroup.memory.limit`, Gauge; `By` | Configured ceiling, not entitlement/reservation/available bytes. `max` is explicitly unbounded; MemTotal is not an equivalent finite value. |
| `pids.current`, group/subtree | `shards.cgroup.task.count`, observable UpDownCounter; `{task}` | Counts kernel TIDs, including threads/descendants. Neither `system.process.count` nor one process's `process.thread.count` is truthful here. |
| `io.stat:rbytes/wbytes`, group/subtree | `shards.cgroup.disk.io`, observable Counter → cumulative monotonic Sum; `By`; `disk.io.direction=read|write` | Keep real device detail or explicit aggregation. Host-backed share I/O outside this group is not measured by this counter. |
| Guest `/proc/stat`/online CPU inventory | `system.cpu.time` Counter `s`, actual `cpu.mode`; `system.cpu.logical.count` UpDownCounter `{cpu}` | Collect real fields/tick conversion. Existing collapsed `system` total cannot recover modes or physical-host CPU time. |
| Guest `/proc/meminfo` | `system.memory.usage` UpDownCounter `By`, actual `system.memory.state`; `system.memory.limit` UpDownCounter `By` | Follow the convention's whole-system formula; cgroup memory.current is not a replacement. |
| Guest `/proc/net/dev` | `system.network.io` Counter `By`, `network.io.direction=receive|transmit`, actual interface where retained | Interface observations, not authenticated tenant/tool traffic. Existing aggregate excludes loopback and loses interfaces; it cannot prove all-route interception. |
| Actual process native reader | Relevant `process.cpu.time`, memory/disk/thread/page-fault/Unix-FD/Windows-handle instruments | Apply each exact convention only to the actual measured process; keep host helper and guest process identities separate. |

Kernel basis: pinned [cgroup contract](https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/Documentation/admin-guide/cgroup-v2.rst)
and [proc contract](https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/Documentation/filesystems/proc.rst).
Standard mappings require the [system](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/system/system-metrics.md)
and [process](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/system/process-metrics.md)
instrument/attribute requirements. For a truthful container subject,
`container.cpu.time`, memory usage/working-set and disk IO are alternatives;
renaming does not repair a false subject.
[container metrics](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/system/container-metrics.md)

General selection: observable Counter observes absolute cumulative counters;
observable UpDownCounter observes additive current quantities; Gauge measures
non-additive instantaneous values; distributions need Histogram or
ExponentialHistogram, not guessed local latency buckets. Units belong in
metadata: `s`, `By`, `1` for fractions, explicit count annotations.
[API instrument selection](https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/metrics/api.md),
[units](https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/general/metrics.md)

## 4. Timestamps, resets, missing observations and provenance

TimeUnixNano is the actual nonzero Unix-nanosecond sample time. StartTimeUnixNano
may be **omitted when truly unknown**. Existing kernel cumulative counters can
therefore remain cumulative monotonic Sums without a warm baseline/cache or
invented origin. Do not describe zero as a known epoch start. NumberDataPoint
integers are signed 64-bit: check kernel-u64 conversion, never wrap. CPU seconds
can use double with disclosed precision. Custom unsigned generation attributes
can use consistently typed decimal stringValue rather than overflow AnyValue's
int64; do not switch attribute types at the boundary.
[timestamp/number contract](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/metrics/v1/metrics.proto#L162-L187),
[AnyValue](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/common/v1/common.proto)

Unknown start weakens reset/rate interpretation. A true lifecycle-recorded origin
can later be used consistently. The model's zero-duration unknown-start first
point requires retained continuity afterward; setting start=sample on every
request instead asserts repeated resets. Do not add a duplicate baseline store
to conceal uncertainty. Cgroup recreation, process reuse and snapshot rollback
need explicit generations/reset handling; overlapping writers must not merge.
[temporality/resets](https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/metrics/data-model.md#temporality)

Unsupported, missing, unreadable or overflowed measurements are not zero. Omit
unavailable points with independent typed collection diagnostics. Flag 1
(`NO_RECORDED_VALUE`) is OTLP's explicit missing/staleness mechanism, but the
same schema calls an absent NumberDataPoint value-oneof invalid. Do not assume
flag 1 plus no value is a settled exception without conformance proof. Official
Collector core v0.165.0 `e883d737e49935984df6b064e512eed6cdc0d289` represents
ValueTypeEmpty and parses JSON without this semantic check; that proves a parser
representation, not all downstream handling. Initial metric omission plus typed
diagnostics is unambiguous. A paused LAST_USAGE sample remains historical at
its original time, never a fresh observation.
[flag/value-oneof](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/metrics/v1/metrics.proto#L365-L428),
[Collector point type](https://github.com/open-telemetry/opentelemetry-collector/blob/e883d737e49935984df6b064e512eed6cdc0d289/pdata/pmetric/generated_numberdatapoint.go),
[JSON parser](https://github.com/open-telemetry/opentelemetry-collector/blob/e883d737e49935984df6b064e512eed6cdc0d289/pdata/pmetric/json.go)

Read through the held group/process object where possible and preserve identity/
generation. Sequential kernel reads are not an atomic resource reservation.
The metrics wire lacks a general uncertainty interval or Logs-style
ObservedTimestamp; do not silently replace guest sample time with host receive
time or invent clock accuracy. Keep kernel-observed, manager-accounted,
broker-enforced and harness-reported provenance distinct. These observations
cannot authorize tool effects or reconstruct lost process history.
[existing management contract](../design/managed-runtime-plan.md)

## 5. Standard JSON and protobuf transport options

ExportMetricsServiceRequest contains ResourceMetrics → ScopeMetrics → Metric →
points. Its response is ExportMetricsServiceResponse. OTLP requires no guest
Collector, SDK, periodic reader or timer: an on-demand snapshot can create a
conforming request. Printing request bytes alone is an artifact, not delivery;
a sender must own transport/response handling. No command/topology is chosen.
[service schema](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/collector/metrics/v1/metrics_service.proto)

| Encoding | Exact obligations | Existing seam, unverified cost |
| --- | --- | --- |
| OTLP/HTTP JSON | `application/json`; lowerCamelCase; enum tags numeric; int64/uint64/fixed64/sfixed64 and timestamps decimal strings; trace/span IDs hex; other bytes base64 | serde_json plus registry HTTP/TLS. Generic serde u64/enums do not automatically satisfy OTLP JSON. Size/CPU/allocations remain unmeasured. |
| OTLP/HTTP protobuf | `application/x-protobuf`; pinned field/wire types | Gateway's protobuf is private BuildKit code, not a ready OTLP public module. Any shared seam needs real callers and checked/fallible allocation. HTTP/1.1 does not need a new HTTP2 runtime. |
| OTLP/gRPC | MetricsService Export over gRPC/HTTP2 | Existing gateway uses a BuildKit pipe; that does not prove network gRPC/TLS exporter compatibility. |

ProtoJSON also governs defaults, oneofs, typed AnyValue fields and special
floating-point values. Receivers accept numeric or string 64-bit inputs, but
senders encode decimal strings. OTLP deviations require integer enum values,
camelCase-only keys, hex trace/span IDs and ignoring unknown fields.
[ProtoJSON](https://protobuf.dev/programming-guides/json/),
[OTLP JSON rules](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/docs/specification.md#json-protobuf-encoding)

HTTP exports POST to `/v1/metrics` by default over HTTP/1.1 or HTTP/2, with
matching response content type. Defaults 4318 (HTTP)/4317 (gRPC) do not authorize
ingress, NETWORK bypass or managed-output port reuse. HTTPS must retain existing
TLS/hostname verification. Sender compression is optional.
[transport](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/docs/specification.md#otlphttp),
[exporter configuration](https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/protocol/exporter.md)

Full acceptance: HTTP 200, serialized response, no partial-success detail. Empty
protobuf is valid; JSON's empty object is `{}`. `partialSuccess` reports
`rejectedDataPoints` and diagnostic; zero rejected with warning is distinct.
Empty detail is equivalent to full acceptance. Never blindly replay a populated
partial response: individual rejected points are not identified. Preserve typed
accepted/partial/warning/invalid-response outcomes rather than every 200→success.
[response fields](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/collector/metrics/v1/metrics_service.proto#L43-L88)

OTLP's retryable HTTP error table is 429/502/503/504; other 4xx/5xx are not retried.
Honor Retry-After/backoff; the specified disconnect-without-response case sends
the same request again. Distinguish ambiguous acceptance from known rejection.
Bound whole-operation time and request/response bytes, including decompression,
from admitted resources/caller authority. Protocol-recommended size defaults
are not independently justified Shards entitlements.
[retry/failure contract](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/docs/specification.md#retryable-response-codes)

## 6. Official SDK and native seam comparison

No resolved transitive graph, binary-size delta, allocation/fault count or
platform gate was measured. These are source properties, not speed claims.

| Alternative | Observed properties and integration obligations |
| --- | --- |
| SDK+OTLP default features | 0.33.1 enables HTTP protobuf, blocking reqwest, traces, metrics, logs/internal logs. Generated messages use prost 0.14; SDK metrics enables futures components. HTTP features also pull trace features. Review actual feature graph; unrelated defaults are not justified by this slice. |
| SDK with Shards HTTP client | Public `opentelemetry-http::HttpClient` can accept a custom client. HTTP protobuf need not use tonic gRPC; JSON adds serde_json. Deadline, cancellation, TLS, typed outcomes and resource behavior still need actual integration proof. |
| SDK ManualReader | On-demand collect; this reader has no periodic worker. Public interface is behind `experimental_metrics_custom_reader`. Kernel scope/start/availability must still be truthful. |
| SDK PeriodicReader | Named native thread, mpsc control and retained ResourceMetrics. Source documents callback/export stalls, callback-panic termination and current-thread Tokio shutdown deadlock risk; production poison-lock `expect` calls remain. It is not needed for one snapshot, nor automatically R01-compliant. |
| Native OTLP at existing snapshot/HTTP seams | Can reuse kernel cumulative state without a second aggregation registry or guest exporter process. Shards then owns schema, bounds, typed exporter states and conformance; valid bytes alone do not establish SDK compliance. |

Sources: [SDK manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-sdk/Cargo.toml),
[OTLP manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-otlp/Cargo.toml),
[HTTP trait](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-http/src/lib.rs),
[ManualReader](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-sdk/src/metrics/manual_reader.rs),
[feature gates](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-sdk/src/metrics/mod.rs#L48-L80),
[PeriodicReader](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-sdk/src/metrics/periodic_reader.rs).

Official conversion creates resource/scope/metric/point Vecs; HTTP serializes a
complete Vec, retains request data in Arc and clones the body for attempts.
This revision pretty-prints JSON. These are concrete copy/allocation sites,
not measured cost. HTTP metrics logs partial rejection/response parse failure
and returns Ok after transport success: Shards' typed outcome needs an explicit
integration decision.
[transformation](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-proto/src/transform/metrics.rs),
[HTTP encoding/retry](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-otlp/src/exporter/http/mod.rs),
[metrics result handling](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-otlp/src/exporter/http/metrics.rs)

Retry source has a SystemTime-before-epoch unwrap and states that the retry
deadline does not cancel an in-flight operation. Upstream retry counts/delays
or reader intervals are not Shards policy. These source observations require
live proof before any defect/fix or safe-adoption claim.
[retry implementation](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-otlp/src/retry.rs)

Shards already has [Request/Response, cancellation/proxy/TLS](../../crates/registry/src/http.rs),
[TLS configuration](../../crates/registry/src/tls.rs),
[serde_json](../../crates/registry/Cargo.toml) and
[private native protobuf](../../crates/gateway/src/wire.rs).
Reuse does not establish whole-operation deadlines, fallible encoding or resource
bounds. Ordinary Vec/serde growth can still abort. Native-vs-SDK selection needs
the actual selected caller's conformance and cost measurements.

Established readers provide independent semantics: opencontainers propagates
separate CPU/memory/IO/task read errors; cAdvisor maps pids-controller current
to thread counts and derives working set separately. Collector hostmetrics
declares native process counters; dockerstats reads a different runtime/API.
These are comparison baselines, not extra installed Shards services.
[cgroups readers](https://github.com/opencontainers/cgroups/tree/e0c56cb31dcb3cb2b3d1554b20dd01ced32e2a2b/fs2),
[cAdvisor](https://github.com/google/cadvisor/blob/7cbdee9260c4292d5d27d00416ce8a7dc1dc12a0/container/libcontainer/handler.go),
[Collector process metadata](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/6083b2c53d78b21d0925b703194aaed920c01af5/receiver/hostmetricsreceiver/internal/scraper/processscraper/metadata.yaml),
[Docker metadata](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/6083b2c53d78b21d0925b703194aaed920c01af5/receiver/dockerstatsreceiver/metadata.yaml)

## 7. Bounded observation and mandatory audit stay separate

Not every counter emits change events. Notifications on specific cgroup state/
threshold files do not make CPU time or every memory byte event-driven.
Request-driven reads fit current stats. A future cadence needs explicit derived/
requested authority and cancellation; OTel does not require polling/cache refresh.

Admit identities/series from authorized resource inventory. Bound retained state,
payloads, pending exports and attributes by measured object cost within existing
hierarchical budgets. Learning cannot enlarge authority. Paths, prompts, argv,
credentials, arbitrary labels and per-flow/per-call IDs must not become unbounded
dimensions. Keep only necessary resource identity; removing dimensions must not
merge concurrent writers. Hashing labels does not reduce cardinality or itself
anonymize them.

SDK cardinality overflow uses `otel.metric.overflow=true` with defined aggregation
semantics. Its suggested default count is not a Shards entitlement. A native
snapshot's typed omission/refusal is different from SDK overflow aggregation;
do not claim the latter just by attaching a label.
[SDK cardinality contract](https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/metrics/sdk.md#cardinality-limits)

An explicit candidate exporter flow is collect → encode → send → accepted/
partial/rejected/uncertain/cancelled; retries require classified outcome and
remaining admitted resources. No lifecycle/control locks across network I/O.
Collector failure cannot create an unbounded queue, block stop/reap, or recurse
into unbounded metrics about exporter errors. Optional loss is an observation
gap, not permission to relax enforcement.

Mandatory durable pre-effect audit remains its own admission obligation.
OTLP acceptance is not an end-to-end durable audit guarantee; counters cannot
prove every effect was recorded. OTel's optional error handling favors preserving
application behavior and cannot silently weaken P7/P9 audit requirements.
[OTel error-handling scope](https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/error-handling.md),
[Shards audit contract](../design/managed-runtime-plan.md)

## 8. Coordinator decisions and live validation still owed

1. Select actual caller/subject schema and JSON/protobuf/SDK after equivalent
   encoder+Collector data, including resolved dependencies/runtime/binary cost.
2. Collect typed availability, held-object identity/generation and sample time
   before the display-defaulting path; verify against concurrent runnable/files-only
   AGENT/HARNESS members and actual live changes.
3. Decode selected wire at pinned Collector; check values/types/units/schema,
   integer boundaries, decimal-string timestamps, enums, oneofs, flags, unknown
   fields, duplicate attributes and malformed responses.
4. Prove unknown/known start, recreation/PID reuse, pause, restore/move, clock
   jumps and overlapping writers produce no fabricated current zero/reset/sample.
5. Exercise full/partial/warning/invalid response, permanent/retryable errors,
   Retry-After, ambiguous disconnect, slow peers, deadlines/cancellation and
   decompression bounds. Observe actual Collector receipts and no partial replay.
6. Measure per-tenant/aggregate bytes, series, FDs/threads and surviving stop/reap
   progress under unavailable/stalled export. Separate observation delivery from
   mandatory audit retention/admission.
7. Root profiles paired equivalent native/official SDK and established baselines:
   allocations/reallocations/copies, page faults, resident/kernel cost, CPU,
   throughput and latency distributions. No 4–5× result was measured here.
8. Seven-target lint/build plus actual authorized KVM/HVF/WHP gates. Guest Linux
   collection spans both architectures; exporter transport stays shared across
   Linux GNU/musl+Windows amd64/arm64 and macOS arm64. Paused Windows ARM stays paused.

## 9. R01–R16 evidence accounting

| Rules | Evidence/remaining work |
| --- | --- |
| R01/R04/R06/R07/R13 | Documentation only; no production path/state/abstraction introduced. Source panic/allocation/deadline/typed-outcome limits recorded without live defect/fix claim. |
| R02/R03/R05/R12 | Exact primary contracts and established alternatives above. No runtime number/cache/polling loop/topology chosen. Selection still requires live data. |
| R08/R09/R10/R14 | No performance/completion or bug-fix claim; actual conformance, reproductions and profiling remain coordinator-owned and unrun. |
| R11 | Seven-target/real-VM gates listed, none run here; Intel Mac unsupported and ARM64 Windows pause preserved. |
| R15/R16 | Isolated branch/worktree from main33df; this file only. No children/builds/campaigns/cache deletion/credentials/main merge. Policy/diff check outcomes accompany the signed return. |
