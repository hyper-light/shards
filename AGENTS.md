# Shards agent instructions

**Before editing, building, testing, or delegating, read
[docs/development-rules.md](docs/development-rules.md) in full.** Its R01–R16 rules
are binding on the coordinator, every subagent, and resumed/future sessions.
Read [CLAUDE.md](CLAUDE.md) for repository mechanics and the active plan/handoff
for current work. Current user instructions take precedence over older files.

Record the policy revision/hash and your role before starting. A coordinator must
include the policy path and required readback in every delegation; an implementer
must acknowledge it before edits. After compaction/resume, re-read the policy and
the current handoff. An older checkout, skill, task brief or handoff cannot waive it.

Use [docs/development-evidence.md](docs/development-evidence.md) for the mandatory
decision/reproduction/validation record. Under R15, implementation agents build
scoped pieces and run targeted light checks; assigned adversarial agents also
run bounded live E2E/fault/chaos/stress/mutation campaigns in owned scopes.
The main coordinator owns audits, profiling, benchmarking, integration validation,
evidence review and main merges. Subagents do not merge themselves to main.

Supported targets are [resources/supported-targets.txt](resources/supported-targets.txt).
macOS is arm64 only; Intel Mac is unsupported. Run `scripts/check-development-policy`
before reporting a piece ready. Passing it does not replace the evidence and
coordinator review required by the policy.

## Code review rules

Reject a change that violates any R01–R16 rule or lacks its required evidence.
Verify actual effects, state transitions, allocations, cleanup and platform behavior.
Record unrun tests, unknowns and benchmark misses explicitly; never report them passed.
