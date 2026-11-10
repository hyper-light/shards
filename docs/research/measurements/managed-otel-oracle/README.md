# Official Collector metrics decoder oracle

This is a research test tool, not Shards production code or an installed
component. It consumes one complete ExportMetricsServiceRequest JSON document,
decodes it with the official Collector `pmetric.JSONUnmarshaler`, and emits a
normalized receipt. It starts no Collector, VM, network listener or exporter.
It has no sampling loop, registry cache, production dependency or public CLI
decision.

## Source and policy

Base main `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`; implementer role.
Read AGENTS.md, full R01–R16, development-evidence.md, CLAUDE.md, the management
plan and `/Users/adalundhe/g121-handoff.md`. Rules SHA-256:
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only this directory is owned. Earlier signed research `60beb766b6db81bfb5f846320565c44a52531b21`
is preserved. No children, rejected campaign retries, Windows ARM activity,
privilege/settings changes or main merge.

Official module pin: `go.opentelemetry.io/collector/pdata v1.71.0`, from core
`e883d737e49935984df6b064e512eed6cdc0d289` (core release v0.165.0).
[Go proxy version metadata](https://proxy.golang.org/go.opentelemetry.io/collector/pdata/@v/e883d737e499.info)
resolves the commit to `refs/tags/pdata/v1.71.0`. Its
[go.mod](https://github.com/open-telemetry/opentelemetry-collector/blob/e883d737e49935984df6b064e512eed6cdc0d289/pdata/go.mod)
requires Go 1.26 and slim OTLP v1.11.1. [upstream.json](upstream.json) retains
the resolution, inspected source hashes and release-asset metadata.

The value interpretation is the official
[JSONUnmarshaler](https://github.com/open-telemetry/opentelemetry-collector/blob/e883d737e49935984df6b064e512eed6cdc0d289/pdata/pmetric/json.go),
over the same request hierarchy exposed by
[pmetricotlp.ExportRequest](https://github.com/open-telemetry/opentelemetry-collector/blob/e883d737e49935984df6b064e512eed6cdc0d289/pdata/pmetric/pmetricotlp/request.go).
No custom OTLP value decoder or mock source/Collector is involved.

## Invocation and receipt

Build only inside a caller-owned research workspace/cache:

```sh
go build -o "$owned_dir/oracle" .
"$owned_dir/oracle" -max-bytes "$admitted_payload_bytes" -input export.json > receipt.json
"$owned_dir/oracle" -max-bytes "$admitted_payload_bytes" < export.json > receipt.json
"$owned_dir/oracle" -h
```

`-max-bytes` is mandatory, positive, and must permit a checked one-byte excess
witness. There is no picked payload/cardinality/queue/time default. Stdin is the
default input; `-input` opens the explicitly named file. One bounded read detects
excess input before decoding. The caller still owns the process/memory/time
envelope: this byte bound does not claim a hard bound on Go's decoded-object,
stack or allocation overhead, nor provide a deadline for a stalled stdin.

The JSON receipt contains resources/attributes/schema URLs, instrumentation
scopes/name/version/attributes, metric name/unit/type, Sum temporality/monotonicity,
point timestamps/flags/number kind/value and point attributes. This slice supports
numeric Gauge and Sum only; other metric types refuse explicitly rather than
being dropped. All source 64-bit integers and timestamps become decimal strings;
floating-point values use a round-trip numeric string. Attribute strings remain
strings; maps/slices retain typed children; bytes become base64. Attribute keys
are sorted; resource/scope/metric/point order follows decoded source.

Start timestamp `"0"` in the normalized receipt is the official API's unknown/
omitted default, not a claimed known epoch origin. The tool does not invent a
generation, origin, metric, unit, schema URL or resource identity. It does not
preserve source lexical spelling such as whitespace or redundant default fields.

Only supply authorized, sanitized measurement artifacts. The receipt necessarily
projects their attribute/value data; it is not a redactor or a safe destination
for credentials/raw tool content. Decode/input failures print categorical codes,
never decoder text, input paths or payload bytes. Stdout write failure is terminal
and produces no receipt claim.

## Official parsing versus added oracle checks

The official parser accepts some structures whose semantic validity needs a
separate check. `TestOfficialParserVersusAddedSemanticChecks` first demonstrates
official acceptance, then verifies the stricter oracle refusal, for each case.
The added checks are deliberately identified as oracle policy, not attributed to
the official JSON parser:

- A generic standard-library JSON token pass requires one object document,
  rejects duplicate object keys and multiple known number/metric/AnyValue oneof
  field names at one object level. It reads no OTLP values into the receipt.
  This is stricter than OTLP's unknown-field behavior: a conflicting combination
  inside an unknown extension object is refused too. It is not a general OTLP
  endpoint compatibility claim.
- Official decoded NumberDataPoint must contain an Int or Double value, including
  when flag 1 is present. This avoids assuming that the schema's missing-value
  flag overrides its separate invalid-absent-value rule.
- Resource, scope and point attribute lists must have unique keys. Duplicate
  point attributes remain visible through the official pdata Map.Range API.
- Sample timestamp must be nonzero and not earlier than its start; Sum
  temporality must be Delta or Cumulative. Other metric types refuse explicitly.

The shape and presence checks are not a complete OTLP semantic validator. For
example, this tool does not establish single-writer continuity, counter reset
semantics, real subject identity, producer authority, monotonic samples across
requests or downstream handling. Root owns actual Collector HTTP receiver
conformance, field-loss, generation/reset, missing-data and mutation checks.

## Independently verified Collector binary metadata

These are official release assets; this lane downloaded only the checksum text
and release metadata, never a Collector binary:

- [otelcol_0.165.0_linux_amd64.tar.gz](https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v0.165.0/otelcol_0.165.0_linux_amd64.tar.gz)
- [matching .tar.gz.sha256](https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v0.165.0/otelcol_0.165.0_linux_amd64.tar.gz.sha256)
- Expected asset SHA-256: `78570dd4bd6570bc41eb327869ef913bbf91b87dd8170ebebbf43bec71f6370d`

The per-asset checksum text is the actual release name; no aggregate
`otelcol_0.165.0_checksums.txt` asset was assumed. The text's SHA-256 is in
upstream.json. Downloading/running an actual Collector is coordinator-owned.

## Evidence and remaining gates

Ordinary fixture/light compile runs are limited to a private Linux scope:
one CPU, 1 GiB requested memory, 2 GiB disk and at most 300 seconds per run;
GOMAXPROCS=1 and private Go module/build caches. Actual results/raw logs, exact
source/module/binary hashes, runner and cleanup state are retained here. Source
research/formatting alone is not a passing compile or actual-export gate.

Actual ordinary run: Kiln r3 on adacachy
`451397fcc585f7f7cb9aaf00e42064bb`, private
`/home/adalundhe/managed-otel-oracle-33df-v1`. The earlier Fedora dispatch was
refused for idle-core headroom and did not start. r3 ran under the stated
300-second limit and exited 0. No larger envelope was substituted.

```sh
export GOMAXPROCS=1 GOMEMLIMIT=1GiB GOTOOLCHAIN=local
export GOCACHE="$task_dir/gocache" GOMODCACHE="$task_dir/gomodcache"
go mod tidy
go test -v -p 1 ./...
go build -p 1 -o "$task_dir/oracle" .
go list -m -json go.opentelemetry.io/collector/pdata
```

Five top-level tests and three named semantic subtests passed. The tests include
official parse acceptance before the stricter missing-value/dual-oneof/duplicate
attribute rejections, integer/time precision, Gauge/Sum metadata, invalid/unsupported
inputs, byte bounds, stdin/file equality and help. [Raw unit output](raw/unit.log),
[build output](raw/build.log), [module resolution](raw/module.json),
[environment](raw/environment.log), [download/tidy log](raw/tidy.log),
[before-source hashes](raw/source-before.sha256),
[after-source and binary hashes](raw/source-after.sha256).

The built research oracle is retained at the private directory's `oracle` path,
SHA-256 `3bd2dc243199d752560f2196e3aaf3b39a3829c792c6989fdf7583cc42996a10`.
Its tested main.go/main_test.go and resolved go.mod/go.sum hashes match the local
checkpoint byte-for-byte. Private Go module/build caches were cleaned after
completion, preserving the binary, source and all logs; [cleanup](raw/cleanup.json)
records those checks. No other cache or source was touched. No jobs remain running
for this lane.

No actual Shards export, Collector HTTP receiver, missing/reset/mutation campaign,
VM, fault sweep, performance comparison or seven-target gate was run. Those
remain coordinator-owned. The raw test duration is test-runner output, not a
benchmark claim.

| Rules | Application and remaining work |
| --- | --- |
| R01/R04/R06/R07/R13 | Small research-only synchronous CLI; categorical error variants, checked input bound, no explicit panic. Official Go/runtime allocation behavior is not presented as production panic-freedom. |
| R02/R03/R05/R12 | Pinned official API at an actual oracle caller; no guessed limits, cache, polling or production abstraction. Byte bound comes only from caller. |
| R08/R09/R10/R14 | Tests distinguish official acceptance from added checks and retain actual logs. No performance/completion/bug-fix claim; profiling remains root-owned. |
| R11 | Ordinary Linux light checks only. Root owns actual-export/Collector and seven-target/real-VM coverage. Intel Mac unsupported; paused Windows ARM untouched. |
| R15/R16 | Isolated new branch, this directory only, signed named-path checkpoint. Private caches/logs only; no other agent/source/evidence deleted. No children or main merge. |
