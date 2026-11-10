# Pinned official Rust OTLP encoding reference

This test-only executable emits the same concrete Snapshot corpus as ROOT's
native codec through three selectable encoding paths. It adds no production
dependency, exporter, SDK pipeline, collector, sensor or resource policy.
Base: `28e147c4dda846ca023020e03bc9b00eb2e58e94`; shared source is unchanged from
`3f7c4838b892cbf47e757ea802f5e1c9a3ff1426`. Main policy revision is
`33df95b08ad8f95bddeaa1c48acf0c64386e0eac`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Implementer: `01a1c5be192972d8b05dc7c207cc6fca`; coordinator:
`01a148884a4770fdaf0111f3f792a248`. Only this new directory is owned.

## Runnable contract

From the repository root, with an existing compiler and a private admitted target:

```sh
cargo build --locked --manifest-path docs/research/measurements/managed-otel-rust-reference/Cargo.toml -j2
"$CARGO_TARGET_DIR/debug/shards-managed-otel-rust-reference" native-stream
"$CARGO_TARGET_DIR/debug/shards-managed-otel-rust-reference" official-compact-buffer
"$CARGO_TARGET_DIR/debug/shards-managed-otel-rust-reference" official-pretty-buffer
```

Exactly one mode is required. Stdout is the actual payload; errors use stderr.
Native mode calls ROOT's actual `write` through a renamed path dependency. All
modes invoke the **unchanged** `managed-otel-encoder/fixture.rs` via `include!` and
a crate-name alias, so its Snapshot expressions are not copied here. The official
builder accepts those same public Snapshot values and independently constructs
an official `ExportMetricsServiceRequest`; it never parses native JSON to create
the reference request. ROOT can include exact `src/reference.rs` by module path
with `Failure`, `Scope`, `Snapshot` re-exported at its crate root, then call
`reference::request(run, generation, samples)` from the parent module.

| Mode | Actual operation and included construction |
| --- | --- |
| `native-stream` | Original native codec writes directly to the caller's writer, including its original final LF. |
| `official-compact-buffer` | Official request construction, then `serde_json::to_vec`, then write the complete buffer. |
| `official-pretty-buffer` | Same construction, then `serde_json::to_string_pretty(...).map(String::into_bytes)`, then write the complete buffer. This matches the pinned exporter's serialization pattern; no SDK export API is called. |

The official construction includes its Strings, attribute/point/metric/resource
Vecs, restore-generation formatting and CPU decimal formatting/parsing. CPU uses
the exact decimal microseconds/1,000,000 rational before binary64 conversion,
matching native JSON's numeric interpretation without promising exact retention
of all large integers in binary64. Do not exclude those adapter costs from a
comparison of these paths. CLI argument parsing, fixture construction and stdout
locking are also present when executing this standalone binary.

Generated serde emits unknown `startTimeUnixNano` as `"0"`; native omits it.
ROOT explicitly accepted that unknown-start semantic equivalence. Payloads are
retained **without postprocessing**. Empty/default schema, metadata, exemplar and
attribute fields are also left as the official serializer emits them. Restore
generation remains an unsigned decimal **stringValue**, member IDs and integer
points remain in signed bounds, failures omit unavailable points and preserve the
same diagnostics, and CPU is `asDouble` seconds. It is the VMM restore count, not
an owner-output management generation or a restart-safety guarantee.

ROOT's production init caller already has `BufWriter::new(File)` at the inspected
`run.rs:3444`. An encoder-only counting sink excludes that existing buffer,
collection and VM work; serializer write-call counts are not production syscall
counts. The buffer's separate R01/R03 source observation does not establish a
reproduced defect and is not changed by this fixture.

## Pins and compatibility

Direct dependency: `opentelemetry-proto =0.33.1`, defaults disabled, only
`gen-tonic-messages`, `metrics`, `with-serde`; `serde_json =1.0.151`.
The fixture's own Cargo.lock pins 61 packages including the two local packages;
the actual feature graph is retained in the raw archive. The proto feature graph
does include `opentelemetry`/`opentelemetry_sdk` 0.33.1; compiling those crates is
not an invocation of their SDK pipeline. No tonic transport, HTTP client or
periodic reader is selected by the fixture.

Registry `.cargo_vcs_info.json` actually identifies official Rust commit
`372ebbc74d4dba71bb9820732f57f7316b4e7a1e`. Compiled generated source hashes match
the exact primary-source downloads. The embedded schema gitlink is
`ca839c51f706f5d53bfb46f06c3e90c3af3a52c6`; the native contract names proto
v1.11.1 `b3f75588eb23c5fca62264edd05d382de49beb1a`. They are not identical schemas.
`check-provenance.py` actually compared 35 used/default-emitted field names,
cardinalities, types and tags plus cumulative enum value 2 across both commits.
Its successful deterministic receipt, URLs, sizes and hashes are [PINS.json](PINS.json).
This proves the checked subset, not whole-schema equivalence.

Primary sources: [official proto manifest](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-proto/Cargo.toml),
[generated metrics](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-proto/src/proto/tonic/opentelemetry.proto.metrics.v1.rs),
[integer serializers](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-proto/src/proto.rs),
[HTTP metrics body, lines 709–735](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-otlp/src/exporter/http/mod.rs#L709-L735),
[SDK data construction/getters](https://github.com/open-telemetry/opentelemetry-rust/blob/372ebbc74d4dba71bb9820732f57f7316b4e7a1e/opentelemetry-sdk/src/metrics/data/mod.rs).
See ROOT's [broader alternative research](../../managed-runtime-otel-metrics.md).
The SDK data API does not directly construct this fixed unknown-start corpus;
this reference therefore makes an encoding comparison, not a full SDK comparison.

## Actual scoped light result

Private Linux runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora kernel
`6.19.10-300.fc44.x86_64`, glibc 2.43. New owned root:
`/home/adalundhe/managed-otel-rust-reference-28e-light-v1` with `source`, `target`,
`cargo-home`, `tmp`, `out`. Admission: 2 CPU / 2 GiB memory / 4 GiB disk,
600-second maximum, sync:false. These are requested admission bounds, not measured
peaks or independently verified kernel enforcement. No local build/target/cache.

Actual tools: rustc 1.98.1 `48a229ceaefd4985c50990b14116b6d856af0985`,
Cargo 1.98.1 `797e8a9bc`, rustfmt 1.9.0-stable and Clippy 0.1.98. This is the
explicit existing compiler used for this light run; the repository's rust-version
is an MSRV, not an enforced toolchain pin. `run-light.py` records literal argv,
private environment and every result in [result.json](records/light-v1/result.json).

Actual format, build, 1 CLI unit + 1 full official generated-type decoder equality
test, strict all-target fixture Clippy and Python schema checks all returned 0.
The equality test parses all three actual stdout payloads into the official
request type and compares complete requests, including numeric values and proto
defaults. Python additionally rejects unexpected nondefault fields and compares
the normalized receipt. There were no build/test/schema failures. Initial read-only
source lookups at nonexistent `src/proto/serializers.rs` and
`src/proto/serializers/mod.rs` returned HTTP 404; the actual serializer is in
`src/proto.rs`. No product defect or before/after fix is claimed from that lookup.

| Actual payload | Bytes | Explicit unknown starts | Resources / metrics / points |
| --- | ---: | ---: | --- |
| Native stream | 4,833 | 0 | 3 / 13 / 18 |
| Official compact buffer | 6,500 | 18 | 3 / 13 / 18 |
| Official pretty buffer | 16,108 | 18 | 3 / 13 / 18 |

These are payload lengths from this fixed corpus, not performance samples.
Each normalized receipt SHA-256 is
`8ae7c71c4de36b534b6af01661bdfe7058cbc494fb712333dee6189db6ec7b3d`.
[Actual payloads/receipts](records/light-v1/payloads/) and
[schema result](records/light-v1/schema-check.json) retain their individual hashes.
No timing, allocation, page-fault, CPU or VM measurements were collected. Cargo's
automatic diagnostic durations remain untouched in raw logs, not benchmark data.

Compiled `src/reference.rs` SHA-256
`5e16b6cbf87b221b1e061690c23283197e11571b6aa0535b163bf046df07f4ca`;
main `2febc6d4fc9079f3648970ebef1891a25682248c4154bdc1fade4833578869e7`;
lock `071f738b5e7aa3692743b907665aa6deff80404773d1d6eacd001b47e60b19a1`.
Actual executable: `target/debug/shards-managed-otel-rust-reference`, 5,659,480
bytes, SHA-256 `0f5cb0d7dab83c723bd3042706992a0b8a1e44d9b0fe081913c1f44908b90401`.
It is an unoptimized light fixture binary, not a measurement or installed binary.

Raw [archive](records/light-v1/raw.tar.gz): 23,509 bytes, SHA-256
`79069b134b826e71638d87e61a2887b4b98c080bc9834eb744ef0ff83fc9f301`.
All 54 safe unique regular members were verified; its 53-original manifest excludes
itself. [Archive inventory](records/light-v1/archive.json), original source package,
literal launch, Kiln result and formatted/compiled source identities are retained.
Original log whitespace was not normalized. Only exact formatter output for these
new Rust files was copied back; shared codec/fixture bytes stayed unchanged.

## R01–R16 evidence and remaining coordinator gates

| Rule | Scoped proof and boundary |
| --- | --- |
| R01 | Panic lints retained; strict fixture Clippy 0. Official DTO/serde ordinary allocations are intentionally included in this test baseline and can abort on allocation failure. This is not production panic/OOM proof or adoption. Tests may assert/panic. |
| R02 | Pinned official code, embedded-schema comparison and ROOT's established SDK/native research above; actual pure equivalent-output result. Transport/full-SDK/resource selection remains ROOT's decision. |
| R03 | No production literals/policies. Schema enum, SI factor, five existing observed fields and exact shared corpus derive from protocol/native contract. Runner limits are explicitly assigned. No measured number becomes a runtime constant. |
| R04 | Explicit CLI mode selection; invalid/missing/extra modes return InvalidInput. Existing typed observations/failures are preserved; no lifecycle state is added. |
| R05 | Actual caller is the included ROOT fixture; request module is also requested by ROOT's separate measurement harness. No waits, caches, polling, tracing or manager. |
| R06 | Fixed instrument/observation mapping and direct generated DTO construction. No generic framework. |
| R07 | Comments explain unknown start, numeric conversion and source reuse/cost boundaries. |
| R08 | No performance superiority/4–5× claim. ROOT owes equivalent profiles and all misses. |
| R09 | New reference requirement, not a claimed existing defect/fix. No speculative production fix; lookup 404s disclosed above. |
| R10 | Exact shared sources, compiled hashes, commands, raw outputs, payloads and complete request comparison retained. No guard-removal or actual collector result is claimed by this lane. |
| R11 | Actual light x86 GNU only. Other targets, musl/native host VM behavior and production combined gates remain ROOT-owned/unverified by this fixture. macOS arm64 only; WinARM remains paused. |
| R12 | One unchanged native codec plus official shared generated Rust implementation, no OS adapters or extra installed deliverable. |
| R13 | Only three required encoding modes and their source/schema checks. No production dependency or parallel encoder. |
| R14 | No profiles/benchmarks executed. ROOT owns allocations/reallocations/retention, faults, CPU/distributions, parent/kernel/VM costs and equivalent workload method. |
| R15 | Implementer-only private light scope, no actors/collector/campaigns/children/main merge. Signed named-path checkpoint; coordinator reviews/adopts. |
| R16 | No local build/cache; new remote private directories, no deletion or Root-cache writes. Raw evidence and binary retained. The 30 GiB local floor remains binding. |

ROOT still owes its independent official Go oracle and collector checks, chosen
measurement build/configuration and exact module closure, meaningful applicable
guards/mutations, actual profiles with raw distributions, production/full-SDK
comparability decisions and any combined/platform/adoption gates. This reference
neither changes enforcement nor closes P0–P13/G25. The sustained lane remains open.
