# Required development evidence

Use this checklist with [R01–R16](development-rules.md) for each implementation
batch. Link existing records rather than duplicate them. The coordinator owns
all audits, profiling, benchmarking, heavy real E2E, chaos, sweeps, evidence review
and final combined gates. Agents return scoped implementation and targeted
unit/light integration evidence under R15, with owed coordinator runs explicit.

- **Identity:** task/gap IDs, Git revision, policy SHA-256, coordinator/implementer,
  owned paths, environment/toolchain/kernel/architecture and supported target scope.
- **Decision (R02/R03/R05/R12):** primary sources and revisions; established
  alternatives; compatibility/security/cost comparison; live method/data; chosen
  interface; every literal's source/derivation and cold-start/failure behavior.
- **Wait/cache review (R05):** affected waits/caches, authoritative state,
  notifications versus repeated probes, cache keys/invalidation/bounds, measured
  cost and retain/replace/remove decision; completed refactor and live proof for
  confirmed problems, with remaining work explicit.
- **Live example (R09/R10):** exact repeatable invocation; expected and observed
  symptom; baseline red logs/traces; fixed green result; guard-removal mutation
  that fails for the relevant reason; actual forbidden effect/cleanup observed.
- **State and code (R01/R04/R06/R07/R13):** explicit states/transitions and invariants;
  ownership/cancel/error/revocation paths; panic hazards; complexity review;
  focused source diff and relevant format/clippy results.
- **Adversarial results (R15):** real E2E; deadlock/livelock and fairness;
  fault/chaos/boundary/break cases; malformed/hostile inputs; slow consumers;
  exhaustion; owner/worker/relay death; retry/reconnect/restore/move; resource return.
- **Performance (R08/R14):** pinned equivalent competing baselines and configs;
  raw samples; n, p50/p90/p99/max and uncertainty; CPU, allocation/reallocation/copy
  counts, page faults, resident/retained/kernel cost, throughput, latency and
  neighbor impact; measured 4–5× target ratios and misses without invented allowances.
- **Portability (R11/R12):** supported seven-target results and actual host VM
  coverage, Linux libc/distribution/kernel variants, platform-specific necessity
  evidence, unavailable hardware and skips listed as unverified.
- **Operations (R15/R16):** role/dispatch acknowledgments, signed commit IDs, active
  job/cache ownership, storage cleanup, preserved evidence, remaining owed runs.

Each result names its command, source revision, log/trace/artifact path and outcome.
Protect sensitive content; raw credentials never enter a report or ordinary log.
Record an inapplicable item with its concrete reason; do not use it to waive an
applicable rule. Evidence of an old revision is not a new revision's passing gate.

Before dispatch, include the policy path and demand `POLICY_READ` acknowledgment.
Before readiness, the implementer supplies the above light-test/source record and
all owed coordinator work. Before a completion/performance claim or merge, the
coordinator verifies the applicable records and reports any remaining gap.
