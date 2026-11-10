# Development-policy enforcement evidence

The user's 16 absolute rules are canonical in `docs/development-rules.md`.
AGENTS.md and CLAUDE.md require it before work. Its role/readback/evidence gates
apply to future sessions and every subagent; the coordinator reviews semantic
compliance. The script checks the mechanical subset, never claims to prove that
a model obeyed the text or that runtime code is safe merely from a receipt.

Discovery is based on the official [Codex AGENTS.md instructions](https://learn.chatgpt.com/docs/agent-configuration/agents-md)
and [Claude persistent instruction documentation](https://code.claude.com/docs/en/memory).
Repo files carry guidance to new checkouts. Conditional user-level bootstrap
pointers also direct stale Shards worktrees to current origin/main policy.

Run from the repository root:

```sh
python3 scripts/check-development-policy
python3 docs/research/measurements/development-policy/validate.py
sh -n scripts/lint
```

`validation.log` retains the source/policy identity, environment, baseline and five
actual guard-removal/violation results. These deliberately relax a panic lint,
restore Intel Mac, drop arm64 Windows, remove the policy pointer and omit a rule.
Each must fail while baseline and a harmless link-label edit pass. They are live
checker cases, not VM tests.

During development, Python's missing `tomllib` reproduced on this Mac's system
Python. The check now uses only the existing canonical flat lint declarations and
Python standard-library facilities available here; it does not introduce a general
TOML parser or a new Python dependency. A first pointer-removal fixture removed
only the Markdown label while leaving its valid target, so that fixture correctly
did not fail; the retained fixture removes every occurrence of that pointer.

No runtime management code changed in this batch, so real-VM chaos/performance
claims are inapplicable here. All such results for the actual implementation
remain owed to the coordinator under R08–R15. The seven-target policy replaces
Intel Mac support; historical eight-target evidence stays labelled historical.

## Coordinator rule account

POLICY_READ role=coordinator; task=durable R01–R16 formalization; base=07564e06.
The exact policy hash and live environment are in validation.log. Owned paths are
the policy/entrypoints/checklist/checker, supported target declarations, their CI
wiring and this evidence. The capture lane was held with no edits or tests.

| Rule | Applicable evidence for this batch |
| --- | --- |
| R01 | Runtime Rust unchanged; live relaxation of the production panic lint is rejected. Existing compiler lint gates remain mandatory. |
| R02 | Official instruction-discovery sources above; existing repo CI/clippy mechanisms inspected; repo entrypoints plus stale-worktree bootstrap chosen over session-only notes. |
| R03 | Platform set and R01–R16 IDs derive from the user's explicit requirements; no runtime tuning value or complexity cap added. |
| R04 | Agent workflow explicitly holds implementation until policy readback; mechanical checker has pass/fail outcomes and records each case. No runtime state machine changed. |
| R05 | One policy source, small entrypoints, existing CI plus one mechanical check; no generic enforcement framework or new parser dependency. |
| R06 | Coordinator reviewed focused policy-check functions and linear validation cases; no invented complexity threshold. |
| R07 | Code comments/docstrings identify purpose; rule rationale and sources live in documentation. |
| R08 | No runtime performance claim for policy/config changes. The 4–5× comparative objective is retained for implementation measurement. |
| R09 | Missing tomllib observed live; pointer-fixture behavior reproduced and corrected; real deliberate violation cases retained. |
| R10 | Repeatable validate.py, exact commands and validation.log retained beside this record. |
| R11 | Seven-target set is checked against CI/release; Intel Mac restoration and arm64 Windows removal are live rejected mutations. No new hardware result claimed. |
| R12 | Shared policy/checker across hosts; standard library works on this Mac's Python3.9 without platform-specific implementations. Runtime code unchanged. |
| R13 | Single authoritative rule list and concise pointers/checklist; no duplicated product manager. |
| R14 | Runtime allocation/page-fault behavior unchanged; no allocation-free/latency claim. Product profiling remains coordinator-owned and owed for later slices. |
| R15 | Root performed policy review and live guard tests. Sole implementer held clean. Signed named-path commit and coordinator merge follow. |
| R16 | Held builds below30GiB; removed inactive regenerable compiler caches under allowed Projects targets/~/.cache. Disk recovered from14GiB to44GiB; preserved source, credentials and evidence. |
