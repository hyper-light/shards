# Resource lane: Rust OTLP reference continuation

This is an ongoing implementer lane, not management-layer completion. No children,
goals/autoplanning, main merge, profiles, benchmarks, collector or real-VM work.

Policy reread: canonical main `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`,
full AGENTS, R01–R16, development-evidence, CLAUDE, managed-runtime-plan and
`/Users/adalundhe/g121-handoff.md`. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Role: implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`.

Own ONLY new `docs/research/measurements/managed-otel-rust-reference/**` on
`p/managed-otel-rust-reference-v1`, new sparse worktree
`/Users/adalundhe/Projects/shards/.claude/worktrees/managed-otel-rust-reference`,
base `28e147c4dda846ca023020e03bc9b00eb2e58e94`. Shared codec/fixture bytes match
`3f7c4838b892cbf47e757ea802f5e1c9a3ff1426`; do not edit them. Old resource branch
`p/managed-resource-admission-v1` at `92b937b5286dc976b11ac2db58cd4c23f56e1836`
is clean and preserved; older allocation/startup/paired work stays frozen.

Official proto `=0.33.1`, Rust commit
`372ebbc74d4dba71bb9820732f57f7316b4e7a1e`, embedded schema
`ca839c51f706f5d53bfb46f06c3e90c3af3a52c6`. Native mode calls the original codec.
Official modes independently construct its generated request from the exact
original Snapshot inputs included by path. Generated serde retains unknown start
as `"0"`; ROOT explicitly accepted semantic equivalence to omitted. No payload
postprocessing or intermediate JSON tree. Pretty mode uses the pinned exporter
pattern `to_string_pretty(...).into_bytes()`; no SDK API/pipeline is invoked.
Keep CPU decimal-format/parse and DTO/string/Vec construction costs disclosed.

Only private Linux light checks are assigned: runner
`8fadc1ec213332e16cb0d915a3fb913f`, sync:false, 2 CPU / 2 GiB RAM / 4 GiB disk,
at most 600 seconds, NEW `~/managed-otel-rust-reference-28e-light-v1` with
`source`, `target`, `cargo-home`, `tmp`, `out`. No local targets/caches/builds.
First light request completed 0: format/build/1 unit + 1 complete official-type
equivalence test/strict Clippy/schema checks. No repeat/broad tests are requested.
Actual formatted Rust files and Cargo.lock were copied back byte-exactly and their
compiled hashes verified. Full raw archive has 54 verified unique regular members,
23,509 bytes, SHA256 79069b134b826e71638d87e61a2887b4b98c080bc9834eb744ef0ff83fc9f301.
reference.rs SHA256 5e16b6cbf87b221b1e061690c23283197e11571b6aa0535b163bf046df07f4ca;
lock 071f738b5e7aa3692743b907665aa6deff80404773d1d6eacd001b47e60b19a1.
Payloads native/compact/pretty are 4,833/6,500/16,108 bytes and official decoded
requests match. Raw logs/commands/provenance/payloads are under records/light-v1.

ROOT owns `managed-otel/profile/**`, source-by-path measurement harness, official
Go oracle comparisons, actual Collector, all allocation/fault/CPU/distributions,
supported-platform/heavy/combined gates, production adoption and main merges.
No instrumentation/profiling hooks were requested for this fixture. No new
production dependency/export API/sensor/cache/cadence/resource/guarantee policy.
Do not touch ROOT targets, protected frozen artifacts or paused WinARM.

Next: run the required small policy check; sign named paths via gits with required
Claude Opus 5.5 trailer, push own branch and send ROOT this runnable checkpoint
immediately. No new repeat/broad tests or source changes are needed for that gate.
Continue scoped queue after that checkpoint; no stale-objective lane release.
