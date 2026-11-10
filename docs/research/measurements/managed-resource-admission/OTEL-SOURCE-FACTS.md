# Existing resource producers for ROOT's OTel contract

Source `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`. ROOT owns the production
export contract, integration and live collector/profiling; its peer owns
`docs/research/managed-runtime-otel-metrics.md`. This record supplies source facts
and pure compatibility-parser evidence. It selects no OTel names, units, SDK,
exporter, cadence, external API or owner authority.

## Actual producers and scopes

| Existing seam | Source behavior | Boundary to preserve |
| --- | --- | --- |
| init::run::builtin(STATS) / stats | Selects the workload's cgroup, or a joiner's; emits existing decimal key/value lines. | Workload is `/sys/fs/cgroup/run/workload`; domains are under sibling `/sys/fs/cgroup/run/domains`. Default workload fields are not all-domain/run/VM/parent/kernel totals. `/proc` fields come from the builtin's process context. |
| daemon::usage_of | Requests STATS through exec_quietly, returns None on failed transport/status, parses successful output into Usage. | Whole-request failure is distinct here; per-field availability can already be lost where guest reads defaulted. |
| CLI stats / LAST_USAGE | Keeps last Usage by container ID; the paused CLI path reads it. Prior values/Instant support display rates. | Usage itself contains no source timestamp, generation, freshness or per-field error. Existing cache/stream behavior is not a selected exporter policy or measured cache benefit. |
| engine::watch::sample | Separately obtains guest Usage and optional host VM-process Usage, then host times; guest failure becomes default Usage. | Host and guest scopes differ. Sequential reads do not establish an atomic VM or process-tree snapshot. |
| Existing Docker stats document | Produces compatibility JSON with derived values and placeholders. | It is an existing API, not the proposed metrics interface; literal zeros can be unmeasured placeholders. |
| domains::admission | Strict held-object cap preparation/readback; missing/invalid required values produce typed refusal. | Admission is not observation. Parent::prepare enables/writes controllers/limits, so it is not a read-only collection getter. Caps are not guaranteed owner reservations. |

Exact file hashes and anchors are in `records/stats-authority-source-facts.json`.
This is a focused caller/authority trace, not an audit, live defect reproduction
or a change to polling/caches. ROOT owns selected mechanisms and actual validation.

## Existing numeric information loss

The following are internal compatibility keys/source expressions, not OTel
instrument names or selected metric contracts.

| Data | Current source behavior | Information absent from the number |
| --- | --- | --- |
| cpu | cpu.stat usage_usec multiplied by1000 with saturation. | Missing/read/parse failure becomes0; saturation has no separate status. |
| memory / inactive_file / pids | memory.current, memory.stat inactive_file, pids.current. | Failed/malformed reads become0; zero alone proves neither observation success nor free capacity. |
| limit | memory.max; max or an empty read falls back to MemTotal times1024. | The number does not distinguish cap, unlimited control, unavailable control or fallback physical memory; none supplies an owner reservation. |
| read / write | io.stat rbytes/wbytes summed with saturation. | Device identity and overflow/partial-read status are not retained. |
| rx / tx | `/proc/net/dev` parsed/summed, excluding lo. | Individual interface identity is not retained; the later eth0 label is not an observed NIC identity. |
| system / online | Guest `/proc/stat` values/counts on the raw wire. | Later display/API denominators use host elapsed time; these scopes/conversions are distinct. |

The Docker document derives system_cpu_usage from host monotonic Sample.at times
guest.online. CPU user/kernel split, throttling, num_procs, network packet/error/
drop fields, and block major/minor identifiers include literal0 placeholders.
Those zeros do not prove corresponding native quantities were measured as zero.
Optional host `microvm` currently inserts footprint/resident/cpu_ns, not faults.
Shared Rust field types do not establish equivalent native footprint/fault-counter
semantics across operating systems; no such equivalence is claimed here.

A downstream numeric object alone cannot recover a distinction its parser erased.
Additional trusted availability/provenance must come from the actual producer;
this record introduces no producer or parallel protocol. Parent available/reserved,
control reserve, induced parent/kernel charges and completeness remain unknown.
Neither these stats nor strict cap readback mints a guaranteed-owner claim.

## Seven pure cases and retained failures

Actual GNU light build0; **7 passed,0 failed,0 ignored,0 filtered**; strict private
Clippy0. Cases establish the complete numeric path, absent/explicit-zero equality,
invalid/overflow-to-zero, duplicate last-wins, incomplete lines, ignored unknown
keys and exact numeric u64::MAX. They are offline inputs to the existing parser,
not evidence that normal guests emit malformed wire data or that a VM/exporter
defect was reproduced. The full daemon, usage_of, CLI, engine and collector did
not run in these tests.

Exact source extraction from daemon/commands.rs: Usage declaration lines3353–3367,
298B SHA256 `6d2eba86dac9de23265e89d8e8a54075710844d6b5edd65dc0d1ba5d02213a0f`;
impl prefix containing parse lines3368–3393,807B SHA256
`321a62b9d0b96912bf367fe3878197b2786771afc4a88932707b567337ac486d`.
The private cfg(test) wrapper closes that impl and adds the test child, SHA256
`e3234650760690637b932c230377aef53a9fae228efd219a6aa103f70d939b2c`.
Fixture `stats-authority-fixture.rs` SHA256
`6ae539e099cf4f22219930e353c194a13c60b2397d3e2b05c5fdded79ee5370a`.

V1 failed before Rust because it inherited a nonexistent workspace package version.
V2's manifest-only correction also changed rust-version via a substring match,
again failing before Rust. Both11-member raw sets/inputs/errors remain retained.
V3 copies the known working fixture manifest prefix, replaces only the complete
name line, and preserves explicit fixture version0.0.0 and workspace edition/MSRV.
Production source/manifests/dependencies/providers are unchanged. The local helper
also found tomllib unavailable; no package was installed, and direct archive bytes
supplied the established manifest instead.

Executable7187944B SHA256
`9a3f7118e8f84d630c0a2bdfeaa42c7cc3a95098961f3417ca80598ddaaf7796`.
V3 original archive5194B/19 regular members SHA256
`6b5e778db7d1067ee21ce1f07dc9912b4dbfe92e1a321b0359841d2f9ae365ac`.
All11/11/19 originals, input files, exact source fragments and executable were
verified after the runs. Commands/tool/source/archive identities are in
`records/stats-parser-v{1,2,3}/` and adjacent input/invocation/result records.

Runner8fadc1ec213332e16cb0d915a3fb913f; NEW own remote
`managed-resource-admission-{source,target,light}-33df95b08-stats-parser-v{1,2,3}`.
Serial sync:false1CPU/1GiB RAM/2GiB disk;180s per command; std-only tests with
observed Rust/Cargo1.98.1, exact compiler details retained. No runtime STATS,
proc/cgroup/native sampling, exporter, helper/worker control, allocator refusal,
VM, profile or benchmark executed. All own jobs are terminal; private sources,
caches, binaries and raw remain preserved. No local compilation or cache deletion.
Both rejected campaigns remain HELD, never rerouted through this lane.

The README's R01–R16 account and all ROOT actual/integration/collector/profile/
guarantee gates remain applicable. This pure proof does not certify full-producer
panic/OOM behavior, an exporter/schema change, resource return, collector overhead,
native counter equivalence or management-layer completion.
