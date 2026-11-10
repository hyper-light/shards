# Separate run and shared microVM identity

Status: actual defect reproduced; correction implemented, closing gates pending.
Main a920's ordinary provider case and initial conformance remain historical
passing evidence. They did not test a joined run and do not close this correction.

POLICY_READ root coordinator: main `a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
current AGENTS/rules/evidence/CLAUDE/managed plan/handoff loaded. Root owns this
correction, real VM proof, integration, profiles and main merge. Other agents'
frozen campaigns and the paused Windows ARM VM remain untouched.

## Actual before

`9be1921d44300901c342329c59abbbe436e1e8b1` adds the ordinary test
`metrics::a_joiners_metrics_distinguish_its_run_from_the_shared_microvm`.
Product bytes are still main a920/28e. Normal carried-helper build passes, then
the real KVM test fails101 in11.45s at the actual joiner microVM identity assertion.
The independently read `/proc/sys/kernel/random/boot_id` values are equal in both
workloads. Provider and joiner detached run IDs differ, yet the exported joiner
`shards.microvm.id` equals its joiner run ID, not the shared VM's provider ID.
There is no separate run identity attribute. The exact raw JSON, boot readbacks,
IDs and log remain under root Fedora `~/root-managed-otel-joiner-red-9be-v1`.
Normal owner cleanup returns; observerFD7→7. This is an identity error, not a
counter value/permission or process-isolation conclusion.

The first fixture at `e37e3a699` accidentally used `confined cat`, whose workload
deliberately stays alive. Its131.69s failure precedes metrics collection and
proves no identity defect. `9be` changes only to the existing single-shot `slurp`
reader. Original failed fixture/commands/logs remain in
`~/root-managed-otel-joiner-red-e37-v1`; the corrected probe uses a new directory.
No larger timeout, skipped assertion, new helper or product change made it reach
the actual subject-identity assertion.

## Existing authority and smallest correction

The [pinned OTel resource model](https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/resource/data-model.md)
defines the observed entity through its identifying attributes. Different run
workload groups in one VM must remain distinguishable without claiming a separate
VM. This is the same subject rule and alternative research already recorded in
the [standards report](../../../managed-runtime-otel-metrics.md#2-preserve-the-measured-subject).
OTLP [Resource](https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/resource/v1/resource.proto)
allows both distinct attributes; no new protocol or semantic-convention entity is
invented. These remain Shards custom attributes, without a fake schema URL.

`Daemon::join_vm` already resolves a joiner chain to the real tracked provider
before admission and retains that ID in `Ready::joined`/`Tracked::joined` alongside
the held VM/socket. Use this actual relationship during metrics request formation.
Calling the separate request-file walker `microvm_of` would add disk reads and its
unrelated picked64-depth guard; a new identity cache would duplicate authority.
Neither is needed. These alternatives were inspected, not silently selected.

The owner request now supplies run and provider IDs through the same private
METRICS builtin; both are checked canonical ID strings. The serializer emits
`shards.run.id` and `shards.microvm.id`. For a provider they are equal; a joiner has
its own run ID and the same provider VM ID. Scope remains `workload` for the
joiner, and provider/member counters are not collected through that request.
No collection grant, endpoint, process, timer, cache or extra identity store is
introduced. Existing restore count remains VMM lineage, not management epoch.

The codec and official generated-request reference take both IDs directly. Their
fixtures use the same explicit input values; old payload/raw/profile files stay
unchanged at their exact old source revisions. Historical profile ratios do not
automatically apply to the added identity field or caller argument.

## Required closing results

Retain this red and exact corrected KVM provider/joiner cases, original separate
capture/control regression, strict unit/caller/seven-target gates, actual official
decoder/Collector on corrected exports and a meaningful identity-guard removal
with exact restoration. Preserve current source/artifact/cleanup witnesses and
any failed intermediate harness/build result. Update cost evidence at the new
codec/reference source without relabelling the old measurements.

The change uses checked existing64-hex identity bounds, two actual required
arguments and fallible argv copies. New native-I/O/setup failures return errors;
no extra chain bound, guessed metric, authority or implicit lifecycle state is
added. Full parent/kernel charges, clock/reset/recreation/restore/move, physical
VM totals, production OTLP delivery and remaining P0–P13 stay open. No passing
skip, encoding result or source observation replaces those gates.
