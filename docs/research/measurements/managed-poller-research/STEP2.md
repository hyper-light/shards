# Poller/inotify research checkpoint return

Scope: Step 2 of the ten-step constants/derivation plan
`01a1c5cfb0f6736f910c8f76dcbfc37f`. This is a research return for coordinator
review, not candidate selection or acceptance of a runtime change. The parent
maintenance lane and the managed-runtime plan remain incomplete.

## Identity and policy readback

POLICY_READ implementer, step session `01a1c5d8a67177769d8ab07fbe391e33`:
before edits/checks, read full checkout and current `origin/main` AGENTS,
`docs/development-rules.md` R01–R16, `docs/development-evidence.md`, CLAUDE,
`docs/design/managed-runtime-plan.md`, and `/Users/adalundhe/g121-handoff.md`.
Checkout/remote source base is
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`; documentation parent is
`6eb84cf671278e140f8018768896f408324e0b28` on `p/managed-poller-research`.
`git ls-remote origin refs/heads/main` independently returned the same base.

Rules SHA-256:
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`;
AGENTS:
`5fd5c2bc14079994c7390926c8f3865efdfedf066faa9c33d7dc361697aeea28`;
evidence checklist:
`a84c60f20e3a06a294bd5b091c1f43cfd5cc4b33ef1bd22a50cd94cea07bb97b`;
current handoff snapshot:
`c561caf085fe38b685c0826ffda0776dcb709b8ceee4c0dd78da842efc1461ae`.
The old README/manifest handoff discrepancy is disclosed in the
[historical receipt paragraph](README.md#checkpoints-and-current-handoff).
It does not change the matched policy or source bytes.

Owned paths are only `docs/research/managed-poller-capacity.md` and textual
records in this directory. Coordinator `01a148884a4770fdaf0111f3f792a248` owns
audits, all runtime campaigns, profiles, benchmarks, combined validation and main
merges. No children, production edits, builds, runtime fixtures or runner jobs
were started. Existing no-build/headroom and paused-Windows-ARM holds remain.
The research skill's primary-source/Markdown workflow is used; the explicit
no-child scope overrides its background-agent instruction.

## Returned revisions and signature limits

| Checkpoint | Exact revision | Contents |
| --- | --- | --- |
| Poller | `df37d254f366e8a7c9d75c876d816927860ee620` | Native batch versus reused token Vec; native ABI/count bounds, callers, alternatives and unrun proof/cost design. |
| inotify | `c0db0f6ccd6780d9c703e1ae936674f1795e288f` | Conditional unnamed-record framing; complete-prefix, error/overflow/invalidation, authoritative cgroup state and alternatives. |
| Contract continuation | `6eb84cf671278e140f8018768896f408324e0b28` | Length-relative reservation; logical service versus retained capacity; pre-dequeue preparation and explicit decisive cases. |

All three existing commit objects contain `gpgsig` headers and the required
Claude Opus 5.5 attribution. Their historical signed/pushed receipts remain in
Git and the manifest. The actual fresh `gits verify-commit` attempts returned 1
for each: `kiln-git-sign` says Kiln checks signatures against its root and Git
cannot check them here. This is an unavailable local verification mechanism,
not a successful cryptographic check or a demonstrated invalid signature.
The [fresh command record](checks-step2.json) retains the exact diagnostic.
Coordinator/Kiln signature acceptance remains separate from these light checks.

## Evidence classes and scope of each result

| Class | Retained evidence and what it establishes |
| --- | --- |
| Current source observations | [Note sections 1–2 and 6](../../managed-poller-capacity.md), pinned local citations, and all 17 manifest blobs. Current Git/available working-file bytes match the base. `wait` clears caller storage, uses a native batch of 64 and extends tokens; `relieve` discards reads from a 4096-byte array before rereading state. No runtime failure follows merely from reading those paths. |
| Derived design obligations | Note sections 4 and 8: separate logical C from capacities N/T; reserve against length before possible oneshot dequeue; charge retained/growth storage. For inotify, conditional H bounds one unnamed record, while byte length/read turns are separate service choices. These are inferences from pinned contracts, not executed candidates. |
| Historical coordinator results | The linked [r518/r523 record](../managed-coordinator-output-exit/README.md), including its actual `ebbda9b3` source identity and limits. This lane only checked that record's bytes. It did not rerun or independently audit its host-exit/FD witnesses, and those witnesses prove no capacity scaling, fairness, OOM, allocation or kernel-charge result. |
| Fresh light checks | [checks-step2.json](checks-step2.json) records actual commands and outcomes; [checks-step2-sources.json](checks-step2-sources.json) records matching local blobs, pinned URL/revision structure, upstream raw bytes/archive members and document links. `verify-sources.py` makes these identity checks repeatable. Hash equality is not semantic certification. |
| All unrun work | Note sections 5, 7 and 8 propose coordinator cases only. No compile, unit/integration/E2E, syscall/cgroup fixture, refusal/fault injector, mutation, real guest, seven-target lint, audit, profiling or benchmark ran in this return. No method, growth rule, architecture or replacement number is selected. |

The existing [R01–R16 table](README.md#r01r16-account-for-both-research-batches)
accounts for every rule individually and remains applicable to this documentation
return. Its runtime, portability, performance, ownership and combined-review gaps
remain owed. Current source identities and actual documentation checks supplement
that table; neither a receipt nor a mechanical pass closes those gaps.

## Coordinator candidate-proof checklist — every case unrun

| Required distinction | Explicit case and independent witness in the note |
| --- | --- |
| Length-relative reservation | Section 8, “Capacity argument semantics”: clear retained G, choose C with G < C and C − G <= G; record length/additional/resulting capacity. The capacity-relative mutant can reserve nothing useful. |
| Retained capacity exceeds service length | Section 8, “Retained space exceeds service grant”: N > C, T = C, more than C actually ready; record requested syscall count and actual return. Only n <= C satisfies that prepared contract. |
| macOS refusal/oneshot ordering | Section 8, both refusal cases, plus section 5: child identity/status and actual first dequeue independently witnessed; refusal before dequeue must preserve observation/reconciliation. A later empty wait alone is insufficient to prove loss. Also cover registration-before/after exit and empty ExitWatch drop. |
| inotify complete-prefix/short buffer | Sections 7.2–7.3 and 8: retain raw bytes, actual read returns and independent unread readiness; first record too large stays queued, successful prefix may leave another complete record. A separate named-record fixture challenges only the unchanged file-watch premise. |
| inotify invalidation/overflow | Sections 6 and 7.4: pending-after-removal, IN_IGNORED/UNMOUNT, owned identity/re-registration or explicit terminal failure; authoritative-state reconciliation differs from watch restoration. Injected overflow proves consumer handling only. |
| Cancellation and control progress | Sections 5.3–5.4, 7.5 and 8: sustained hot sources plus quiet/wake/stop/deadline work, pre-wait allocation and drain/rescan work, actual neighbor/control effects and owned cleanup. No numeric latency allowance or endless drain rule is selected. |

Cold setup/refusal, checked count/layout/byte arithmetic, EINTR, partial setup,
concurrent registration/close/dup/reuse, initialized-prefix lifetimes and optional
growth failure are also explicit in the note. A coordinator must approve a
concrete candidate and reproduce its exact claimed symptom/goal at a named
caller before a production fix. Paired costs, relevant target/real-host coverage
and meaningful remove/restore guard witnesses remain coordinator work. No
proposed case above is represented as run or passed.
