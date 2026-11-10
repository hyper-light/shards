# Managed-runtime OpenTelemetry metrics

Status: the on-demand OTLP artifact passes its real KVM caller, the official
decoder/Collector and the seven-target compilation gate at `28e147c4d`.
[Validation](VALIDATION.md) records the exact scope and remaining gates. This is
a G25/P9 slice; the full management layer and production HTTP delivery remain open.

POLICY_READ: coordinator, main `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Read AGENTS.md, full R01–R16, development-evidence, CLAUDE.md, the managed-runtime
plan and current `/Users/adalundhe/g121-handoff.md` before this work. Root owns
integration, audits, collector conformance, resource measurements and main merges.

## Observed baseline

The frozen normal carried binary at code `0219ab2eb` (production bytes identical
to main33df) has no `read metrics` command: actual Fedora invocation
`1-shards read metrics -h` returned 2; `read processes -h` returned 0. The exact
binary SHA-256 is `5b3dfc4350ea698ec6eac0c00090c1ee24399e852b0eeb2d7ef0713b1cd178e8`.
Raw commands/results are retained on the root-owned Fedora runner under
`~/root-managed-otel-baseline-33df-v1/record.json`. An initial metadata recorder
failed before either command because Python's uname_result has no `_asdict`;
the corrected recorder uses a list. That failed recorder is not a product result.

The existing Docker-compatible STATS path reads workload counters, substitutes
zero for failed reads and retains a paused VM's last sample. These are source
observations, not reproduced defects or telemetry guarantees. New OTel observations
must not silently inherit either behavior.

## First slice under evaluation

Read actual cgroup objects on demand through the existing trusted init builtin
and owner transport. Export OTLP metrics using its pinned schema. Initial values
are cgroup CPU time, charged memory, task/thread count and block-I/O bytes. Keep
run, workload and stable declared member scopes distinct. These are cgroup
aggregates, not individual process metrics or all physical VM/kernel costs.
Use established semantic names only where their definitions actually match.

Evaluate native streaming OTLP JSON against the official Rust SDK/exporters;
retain source/dependency, collector compatibility and measured resource evidence
before adoption. No extra guest sensor, periodic collection loop, cache, queue,
public listener, implicit grant or arbitrary collection interval is authorized by
the wire format. Export failure never changes admission or effect authority.
Missing counters retain typed causes and no-recorded-value semantics; cumulative
counters with genuinely unknown birth time do not invent a reset timestamp.

Protocol sources: [OTLP 1.11.1](https://opentelemetry.io/docs/specs/otlp/),
[pinned metrics schema](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/metrics/v1/metrics.proto),
[metrics data model](https://opentelemetry.io/docs/specs/otel/metrics/data-model/).
The [standards report](../../managed-runtime-otel-metrics.md) records the SDK,
semantic-convention and protocol revisions, alternatives and precise limitations.
The first resource marker is explicitly the VMM lineage restore count, distinct
from management generation. It does not close restart/recreation or P11 semantics.

## Required closing evidence

Full real callers and collector decoding; actual independent VM/member values;
malformed, missing, overflow, allocation and disconnected-sink outcomes; meaningful
mutations with restored passes; supported seven-target builds and real VM hosts;
CPU, allocations, memory and faults with a pinned equivalent baseline. Every
R01–R16 item remains to be reviewed against those results. A JSON encoder unit
test alone closes none of these gates.
