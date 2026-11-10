# Binding development rules

Authority: Ada's explicit instructions for implementation of the managed-runtime
plan, 2026-11-10 UTC. These rules apply to all Shards work, all agents, and future
sessions. They are absolute. Current explicit user instructions override older
repository instructions; agents cannot invent exceptions or weaken a rule to ship.

## Start, resume and delegation

Before edits/builds/tests/delegation, read this whole file and the current task's
plan/handoff. Record `POLICY_READ` with the Git revision, this file's SHA-256, your
role (`coordinator` or `implementer`), task and owned paths. A receipt identifies
the instructions loaded; it is not proof of compliance. Re-read after a policy
change or context loss. If an old checkout lacks this file, read it from current
`origin/main` before proceeding; fetch that ref if necessary.

The coordinator includes this exact path, all applicable constraints, path
ownership, light-test scope and the evidence-return requirement in every task.
The subagent reads and acknowledges them before implementation. Keep unacknowledged
lanes held. No subagent spawns further agents without the coordinator's instruction.
Any agent discovering a violation records it and reports it immediately; do not
continue a dependent unsafe action or disguise a missing result.

## R01 — Production never panics

Return and handle errors. Review explicit and implicit panic paths: assertions,
unwrap/expect, indexing/slicing, unchecked arithmetic/conversions, allocation,
poisoned synchronization and library preconditions. Keep the existing panic-related
workspace lints denied. Tests may assert/panic; production may not. Lints are one
check, not a proof that every implicit path is safe.

## R02 — Decisions require extensive research and data

Before an architectural or broader implementation decision, inspect external
primary sources and alternative established implementations, then gather the
relevant live data. Record the alternatives, source revisions, compatibility,
security, resource tradeoffs, method and decision in the design/evidence record.
An agent's preference, familiarity, source count or confidence score is not evidence.

## R03 — Arbitrary constants are forbidden

Use online learned/data-derived values, or exceptionally well-researched invariants
and justified protocol/physical/algorithmic bounds. Document each production
literal's source or derivation in its D-entry, including cold-start, uncertainty,
overflow and adversarial-input behavior where relevant. An upstream default or a
number from one benchmark alone is insufficient. Measurements validate methods;
they do not become unexplained runtime numbers. Learning never enlarges authority.

## R04 — State machines are explicit

Represent meaningful lifecycle/protocol states, events, legal transitions, terminal
outcomes and error/cancel paths explicitly. Record and exercise transitions and
invariants. Do not encode a state machine as an undocumented combination of booleans,
sentinel values, incidental ordering or disconnected callbacks.
Keep new refusals, errors, rejections and responses typed through control and wire
handling. Callers match variants; presentation renders text. Preserve affected
identity, cause, retry and uncertainty semantics without logging private tokens.

## R05 — Abstractions are deliberate

Justify an interface by the behavior and real callers/variants it serves. Prefer a
small interface hiding substantial implementation at an existing seam. Avoid
speculative traits, generic frameworks, pass-through layers and parallel managers.
Apply R02 to broader choices; do not create architecture merely to enable a test.

Audit affected waits and caches when implementing or reviewing a path. Prefer
completion/readiness notifications and one authoritative state owner. Distinguish
event waits and required protocol deadlines from periodic state probes. Improve
the underlying algorithm, I/O and ownership before adding a cache. For any cache
retained or proposed, record its measured benefit, authoritative source, keys,
invalidation, resource/lifetime bounds and consistency proof. Existing use is not
justification. Refactor or remove confirmed unnecessary polling/duplicate cached
state with live before/after and mutation evidence; an inventory alone is unfinished.

## R06 — Control complexity

Keep code concise and locally understandable. Use explicit transitions and focused
functions instead of excessive nesting, large branch combinations or hidden
cross-function obligations. Review cyclomatic complexity and affected paths; do
not invent an arbitrary line/nesting/complexity threshold as a substitute for design.

## R07 — Comments explain what code cannot

Use comments for safety obligations, non-obvious invariants, source/derivation and
necessary rationale. Remove narration that only repeats the code. Preserve required
unsafe-code safety explanations and evidence citations.

## R08 — Compete on measured results

Aim to beat leading comparable implementations by 4–5× in resource usage, latency,
throughput and speed. Select pinned, credible baselines and equivalent workloads,
security guarantees and configurations. Publish measured ratios and distributions,
including misses. The target is not an achieved result or a license to weaken
correctness, compatibility, crypto or isolation.

## R09 — Reproduce issues live before fixing them

If an issue is suspected, first reproduce its exact symptom in a live repeatable
example. Capture the command, revision, environment and logs/traces. A source
observation or plausible theory is not a reproduced defect. Do not implement a
vibes-justified fix or claim reproduction from a different failure.

## R10 — Pair changes with durable evidence

Record broader implementations, decisions, fixes and bugs with repeatable live
examples and retained logs/traces. Include before/after and meaningful mutation
evidence for the guard being changed, resource/cleanup outcomes and unresolved
limits. Keep harnesses under `docs/research/measurements/`, decisions in the relevant
design record, and results in `docs/research/platform-measurements.md` or the linked
measurement record. Use [the evidence checklist](development-evidence.md).

## R11 — Supported platforms and architectures

Support Windows and Linux on amd64 and arm64; Linux includes glibc and musl and
coverage of major distribution/kernel environments. Support macOS on arm64.
**Intel Mac is unsupported.** `resources/supported-targets.txt` is the current
seven-target build/lint/release set. Real-VM validation covers supported KVM,
Hypervisor.framework and WHP combinations; unavailable hardware and skipped tests
are unverified, never passed. Preserve explicit host/VM pause instructions.

## R12 — Share implementations

Use the existing `hv`, `arch` and `platform` seams. Do not create hacks or duplicate
functionality for an OS/architecture unless no shared implementation can achieve
the function. Establish that necessity with researched alternatives and live
evidence, then isolate the smallest platform adapter. Do not sacrifice correctness
or compatibility, ship extra installed binaries, use host root or weaken crypto.

## R13 — Simple and concise

Implement the smallest complete behavior that meets the contract. Keep interfaces,
state and ownership clear. Remove redundant layers and code; do not trade away a
required behavior, evidence or error path in the name of brevity.

## R14 — Profile allocations and faults

The main coordinator performs all profiling and benchmarking, and reviews
optimization of
allocations, reallocations, copies, retained memory and page faults, alongside CPU,
latency and throughput. Count induced parent
and kernel costs, not just child RSS. Compare equivalent live workloads, retain raw
results and report n, p50/p90/p99/max, contention and uncertainty. Do not call a path
fast, allocation-free or leak-free without the relevant evidence.

## R15 — Roles, adversarial validation and merges

Use scoped subagents for parallel implementation and targeted unit/light
integration tests. The main coordinator owns **all audits, profiling,
benchmarking, heavy real E2E, chaos, stress campaigns and sweeps**, including
adversarial multi-tenant validation. These are not delegated campaigns. Agents
return owed coordinator runs when their implementation/light checks finish.
This role split follows Ada's current policy bootstrap and supersedes older
instructions permitting broader agent campaigns.

Delegate implementation with exact source/path/runner ownership, resource
envelopes, policy readback, light-test scope and evidence-return requirements.
Preserve implementing agents; completed batches receive
the next scoped assignment as capacity permits. Runtime concurrency and runner
headroom remain binding; never report queued work as running.
Agents return small tested signed commits, evidence and remaining work at each
checkpoint. They do not merge themselves to main. The coordinator
reviews and batches finished work, validates the combined revision, and merges to
main. Never force-push or disable signing. Use the session's signed `gits` command
and named paths; preserve the required commit attribution. Record pending checks
honestly if the user explicitly requests a merge before their completion.

## R16 — Recover runner space without losing data

If a runner fills, reclaim regenerable `~/.cache` entries, build `target` directories
of projects under `~/Projects`, and temporary storage. This authorization persists;
do not repeatedly ask. Preserve user data, attachments, experiments, unmerged
source/worktrees, credentials and evidence, including when someone placed them in
a directory named cache/tmp. Coordinate active jobs before deleting their caches;
never bypass kiln's headroom guard. Keep this Mac at least 30 GiB free, as the owner
previously required, and hold its builds while below that floor.

## Enforcement and handoff

`scripts/check-development-policy` checks policy entrypoints, rule presence,
panic-lint settings and agreement of the supported target matrices. CI runs it.
Existing format/clippy/tests enforce their own checks. Research quality, live
reproduction, explicit state/ownership design, comparative claims and adversarial
coverage require the coordinator's evidence review; a script cannot establish them
by finding words in a document.

Before readiness/merge, account for **every R01–R16 rule** in the evidence record:
the applicable proof, result and remaining gap. No unchecked item becomes passed
through a receipt or a self-attestation. A handoff records the policy revision/hash,
roles/held lanes, exact source/test revisions, completed results, active jobs,
unrun checks and evidence paths. Older handoffs/plans do not override this policy.
