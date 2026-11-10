# Scoped cgroup OTLP JSON encoder

This is an **unregistered implementation candidate**, not a production exporter.
Root owns the actual init reader/owner command, topology decision, Collector
conformance, mutation/E2E, resource measurements, supported-target gates and main
merge. It implements the wire portion of G25/P9; the broader management plan stays
open.

POLICY_READ: implementer `/root/otel_cgroup_encoder`, parent source
`33df95b08ad8f95bddeaa1c48acf0c64386e0eac`, policy SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Read AGENTS.md, full R01–R16, development-evidence, CLAUDE.md, full managed-runtime
plan, `/Users/adalundhe/g121-handoff.md` and root's managed-otel record before
editing. Owned paths are the new `crates/init/src/metrics/otlp.rs` and this new
evidence directory only. No modules, runtime callers or dependencies were added.

## Contract and decision limits

One `write` invocation serializes one `ExportMetricsServiceRequest` JSON object
followed by one newline, into a caller-owned `std::io::Write`. It takes trusted
run/generation identities and already collected cgroup snapshots. It does not
collect, retain, cache, poll, queue, retry, authenticate or authorize anything.
The root-owned producer remains responsible for admission, observation provenance,
clock meaning, sampling boundaries and trustworthy generation/member association.

Each run/workload/member cgroup is a distinct ResourceMetrics entry. A parent's
counters include descendants; these scopes must not be summed as disjoint usage.
They do not represent an individual process or all physical VM/host/kernel costs.
`threads` is retained as the internal field name agreed with the root reader;
the emitted `shards.cgroup.task.count` is subtree TIDs from `pids.current`.

The instrumentation scope is `shards.init`. Custom `shards.cgroup.*` names avoid
claiming standard process/container semantics that do not fit these aggregates.
No semantic-convention schema URL or SDK identity is invented. Generation is
always a decimal **stringValue**, including small generations, because its full
u64 identity range does not fit AnyValue's signed int64.

| Instrument | Unit and data | Meaning |
| --- | --- | --- |
| `shards.cgroup.cpu.time` | `s`, monotonic cumulative Sum, asDouble | Cumulative usage_usec converted to seconds. |
| `shards.cgroup.memory.usage` | `By`, non-monotonic cumulative Sum, asInt | Current charged subtree memory. |
| `shards.cgroup.task.count` | `{task}`, non-monotonic cumulative Sum, asInt | Current subtree task/TID count. |
| `shards.cgroup.disk.io` | `By`, monotonic cumulative Sum, asInt | Cumulative bytes, with read/write `disk.io.direction`. |
| `shards.cgroup.collection.error.count` | `{error}`, Gauge, asInt | Number of unavailable/overflowed measurement fields in this collection; always emitted, including real zero after success. |
| `shards.cgroup.collection.error` | `{error}`, Gauge, asInt=1 | One actual failed observation at the supplied timestamp, keyed by static instrument/direction, categorical error.type and native errno when present. |

Detailed diagnostic points are timestamped failed-observation facts, not persistent
active-error state or monotonic failure counters. Their absence does not establish
an exact recovery time. Error attributes do not change a healthy measurement's
series identity. Unknown counters are omitted, never replaced by measured zero.
An otherwise valid integer outside signed int64 becomes an Overflow diagnostic.

The caller's nonzero Unix-nanosecond timestamp is required. All snapshots are
checked before writing any output, so a later invalid timestamp cannot start a
partial document. Counter creation time is genuinely unknown in this slice;
startTimeUnixNano is omitted rather than reset on each collection. Consequently
downstream rate/reset analysis remains limited until root's lifecycle integration
provides a true creation epoch. Collection is not claimed atomic across files.

CPU conversion formats the exact decimal quotient/remainder of usec/10^6. The
OTLP consumer stores asDouble as IEEE binary64, so large/fractional CPU values
can round; this is not exact integer-usec preservation. Byte/task counters within
int64 remain exact decimal strings. IO failure returns `io::Error` to the caller;
a sink failure may leave a partial document, which must not be treated as an
accepted/retry-safe export. There is no internal export lifecycle state to retain.

Root considered native streaming JSON versus the official Rust SDK/exporter.
This candidate introduces no dependency or aggregation state for pre-aggregated
kernel observations. That is a source fact, **not** a measured resource advantage
or adoption decision. SDK/dependency comparison, actual Collector acceptance,
export acknowledgement/partial-success handling and all transport work remain
root's gates. See the separate managed-runtime-otel-metrics standards report.

## Primary protocol sources and literals

OTLP/proto is pinned to v1.11.1 commit
`b3f75588eb23c5fca62264edd05d382de49beb1a`:
[metrics.proto](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/metrics/v1/metrics.proto),
[common.proto](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/common/v1/common.proto),
[metrics service](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/collector/metrics/v1/metrics_service.proto),
[JSON encoding](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/docs/specification.md#json-protobuf-encoding).
JSON escaping follows [RFC 8259 section 7](https://www.rfc-editor.org/rfc/rfc8259#section-7).
UCUM byte is `By`; `s` and dimensionless annotations follow
[OTel metric units](https://opentelemetry.io/docs/specs/semconv/general/metrics/#instrument-units).

The initial flag1/no-value idea was not assumed conformant: metrics.proto calls
an absent NumberDataPoint value invalid while separately defining the
NO_RECORDED_VALUE flag. Research found that official Collector core v0.165.0
`e883d737e49935984df6b064e512eed6cdc0d289`
[ValueType](https://github.com/open-telemetry/opentelemetry-collector/blob/e883d737e49935984df6b064e512eed6cdc0d289/pdata/pmetric/generated_numberdatapoint.go)
can represent Empty and its
[JSON decoder](https://github.com/open-telemetry/opentelemetry-collector/blob/e883d737e49935984df6b064e512eed6cdc0d289/pdata/pmetric/json.go)
does not validate that semantic constraint. Representation/permissive parsing is
not downstream correctness proof. Root chose omission plus valid separate
diagnostic measurements for this first slice; no production flag1 is emitted.

| Literal or bound | Source/derivation and failure behavior |
| --- | --- |
| aggregationTemporality `2` | Pinned AggregationTemporality CUMULATIVE enum. No unspecified0 is emitted. |
| monotonic true/false | CPU/IO cumulative growth versus additive instantaneous memory/tasks; not an authority bound. |
| timestamps/int64 as quoted decimals | OTLP JSON/ProtoJSON numeric mapping; timestamp0 rejected before any bytes. |
| 1,000,000 and six decimal places | SI microseconds per second and its base10 exponent. Division/remainder have a nonzero compile-time denominator. |
| i64::MAX | NumberDataPoint.as_int signed64 schema; excess is omitted with Overflow, never clamped. |
| five observations | The five concrete Snapshot fields above, not a tunable budget/queue. All failure counts are at most this fixed field count. |
| diagnostic value1 | One failed field observation. Current total counts those actual failures; successful total is0. |
| JSON 0x1f/quote/backslash, four hex digits | RFC8259 string escape grammar; valid UTF8 outside controls is streamed unchanged. All slice arithmetic is checked. |
| enum/static names, units, identity/diagnostic keys | Concrete approved first-slice contract above. No caller-defined metric or error labels. |
| comma, bracket, zero-index tests | JSON/iterator structure, not runtime limits, timing or authority. |

## Actual light validation

On this arm64 Mac, rustc1.98.1/cargo1.98.1, private target
`~/.cache/managed-otel-encoder-check-33df`, Mac lockf and `-j4`:

- 10/10 focused unit tests passed, no failed/ignored/filtered tests. They cover
  scope/identity separation, units/temporality/directions, microsecond conversion,
  early zero-timestamp refusal, empty collection, all JSON controls/UTF8,
  omission/typed failures, int64 boundary, current recovery count and real
  caller-provided short writes/BrokenPipe failures.
- Strict standalone Clippy with the workspace panic/index lints mirrored in the
  no-dependency evidence manifest passed for its library, tests and fixture binary.
- An actual fixture program emitted three resource scopes. Independent Python
  json decoding plus selected schema/value/identity assertions passed. It checks
  one valid numeric value per point, unique point keys/names, numeric enums,
  quoted int64/timestamps, actual zero versus missing, and escaped text round trip.
- rustfmt, git diff whitespace and development-policy check passed. The policy
  script explicitly does not establish evidence/coordinator compliance.

Repeat the light checks from this worktree with `sh
docs/research/measurements/managed-otel-encoder/check.sh`. This Mac recipe uses
lockf; root's supported-target gates own portability. Synthetic fixture values
and test seeds are not observations or runtime policy constants. Raw original
results and environment/source hashes are in `raw/`; no binaries are committed.
The first staged whitespace check rejected libtest's final blank line in unit.log;
the exact original bytes are retained as unit.log.gz with sizes/hashes in
raw-storage.json. They were not trimmed or normalized. This was an evidence-file
whitespace failure, not a product test failure.

## R01–R16 accounting and remaining work

| Rules | Scoped evidence and limit |
| --- | --- |
| R01/R04/R06/R07/R13 | Typed observation causes/scopes; checked timestamp/identity/numeric/string handling and sink errors; local concise formatter, no ownership/control machine, panic assertions confined to tests. Strict Clippy is not a complete allocation/library proof. |
| R02/R03/R05 | Pinned schema/RFC/units and literals above; concrete root-owned caller/interface. No waits, caches, sensors, queue, retry or manager introduced. Broader SDK/topology and actual cost decision remains root's. |
| R08/R14 | No profiling, benchmark, allocation-free/leak-free or 4–5x claim. Root must measure actual caller/parent/kernel/resource costs and compare an equivalent pinned baseline. |
| R09/R10 | New feature, not a claimed reproduced defect. Root's retained actual baseline command lacks read metrics; this record retains repeatable actual light green checks. Real caller/Collector and guard-removal mutation proof remains owed. |
| R11/R12 | Pure shared std implementation, no platform cfg/adapter/dependency. Actual scoped run is arm64 macOS only; all seven-target and KVM/HVF/WHP integration gates remain unrun, including paused Windows ARM VM. |
| R15/R16 | Implementer readback and owned paths above; no children/main merge/audits/heavy campaigns. Private tiny build target, lockf/-j4, actual disk floor above30GiB; source/evidence/credentials and other active caches untouched. Root reviews and merges. |

The module must not be described as integrated/export-ready until root registers
the real producer/consumer, validates an actual Collector plus errors/limits,
retains live member/VM evidence and completes the applicable combined gates.
