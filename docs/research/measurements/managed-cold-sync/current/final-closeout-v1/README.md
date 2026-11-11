# Frozen owned-API correction: final adoption packet

This packet makes the already tested owned State/Tail correction reviewable
against main7a. It changes no production source and reruns no test or campaign.
The completed helper-bounds research is already on main. This is the remaining
unmerged cold-API source identified by the current concise handoff.

POLICY_READ implementer: main `7a897def2ac3bb041da584d1d6854fa066ad7fc2`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
All six policy/plan/target files are byte-identical to the prior full0a6 read;
the latest concise227-line handoff was read in full. Only this owned evidence
directory is written. No fetch/rebase/worktree/build/runner/actor/child/goal or
cache operation was used. ROOT owns adoption, current combined gates and main.

## Exact source, patch and behavior

Candidate is frozen `f08cc06b009158a5eafce00d3bc806b702a8aff7`; proof checkpoint
`ec494ad24905e2f39fe05f77a9513a8fd41df5e9` has identical three product files.
The patch base is main `7a897def2ac3bb041da584d1d6854fa066ad7fc2`. A source
comparison finds main's API/afunix bytes identical to the actual reproduced33df
baseline; that comparison is not a current-main runtime reproduction or pass.

[source.patch](source.patch) selects ONLY:

- `crates/api/src/client.rs`: actual Shared/State/Tail preparation and callers,
  with owning Child return/cleanup and typed synchronization refusal.
- `crates/api/src/lib.rs`: private module registration and typed refusal.
- `crates/api/src/sync.rs`: one private shared synchronization seam; measured
  Darwin necessity, checked native operations and fallible stable preparation;
  existing std selection on Linux/Windows.

The native cold abort was reproduced before binding. Shared prepares both actual
mutex/CV pairs and fallible shared storage before spawn; State remains the sole
terminal authority and Tail retains its drain predicate. The original owning
zero/MAX and adopted borrowed Ready/Until/Unbounded contract are preserved by the
frozen source/proof record. The Borrowed/Attempt span's recorded identity is in
[BINDING.md](https://github.com/hyper-light/shards/blob/ec494ad24905e2f39fe05f77a9513a8fd41df5e9/docs/research/measurements/managed-cold-sync/current/BINDING.md). No standalone unused backend is offered as the fix.
The patch also includes the already tested portability cfg and exact Linux
OpenStat/ENOENT test alternative; PID/start assertions remain.

[manifest.json](manifest.json) records each base/candidate Git blob, bytes and
SHA-256, the exact patch-generation argv and proof-record identities. It also
records revision-specific counts and every unrun limit. The patch is the exact
unified Git diff; blank context markers are preserved verbatim. ROOT can select
these three source files and the relevant proof, without adopting old branch
ancestry or the later r612 observation hooks.

## Existing actual proof; nothing rerun here

| Frozen scope | Actual recorded result |
| --- | --- |
| Native Darwin main33df baseline | Two cold selections SIGABRT(-6), first64B refusal; initializer/connect stack retained. |
| Native Darwin cd0649 binding r54 | Two cold +five ordinary exact cases pass; strict API lib/tests0. Cold cases return typed Allocation then stop/wait and ECHILD. |
| Native Darwin cd0649 mutation/restoration r97 | std-adapter mutant SIGABRT(-6)/64B; exact restored two cold +five ordinary pass. This is historical targeted correctness evidence, not permission to repeat. |
| GNU final portability r250 | Lib52/0/6ignored/0filtered; sim15/0/2ignored/0filtered; strict API+afunix0; musl x86 linkcompile0, execution UNRUN. |
| Darwin final portability r251 | Eight ordinary/affected exact selections pass, strict API lib/tests0; cold allocation refusal was not rerun in that batch. |
| Native Windows amd64 exactf08 r505 | Lib29/0/0ignored/0filtered; sim4/0/0ignored/0filtered; strict API+afunix0. Native owning zero/MAX cfg-excluded; handle-return unverified. |

The exact original commands, source closure, raw errors, binaries and verified
archive indexes are linked by immutable Git identities in the manifest and in
[BINDING](https://github.com/hyper-light/shards/blob/ec494ad24905e2f39fe05f77a9513a8fd41df5e9/docs/research/measurements/managed-cold-sync/current/BINDING.md), [MUTATION](https://github.com/hyper-light/shards/blob/ec494ad24905e2f39fe05f77a9513a8fd41df5e9/docs/research/measurements/managed-cold-sync/current/MUTATION.md),
[Linux/Darwin](https://github.com/hyper-light/shards/blob/ec494ad24905e2f39fe05f77a9513a8fd41df5e9/docs/research/measurements/managed-cold-sync/current/linux/README.md) and [Windows](https://github.com/hyper-light/shards/blob/ec494ad24905e2f39fe05f77a9513a8fd41df5e9/docs/research/measurements/managed-cold-sync/current/windows/README.md).
No new raw verification run or passing current-main gate is claimed here.
Existing archive receipt counts: Linux113payloads+manifest; Darwin108+manifest;
Windows55payloads. Those original containers and failed setup/compile/lint/
collector records remain unchanged. Six fixture joins in ordinary concurrent
cases do not prove the internal drainer joined or that a native CV parked.

## Open review and owner inputs

ROOT must review/select the patch against its actual integration source,
including exhaustive downstream handling of the new public Refusal variant,
then run its required combined matrix/native gates. No current-main integration,
seven-target completion or whole-workspace compatibility is supplied by this
small closure. Native Darwin error interleavings and init/lock/wait/notify/
unlock/destroy failure behavior remain unverified. Native storage can remain
retained on destroy failure; the correction is not called leak-free. Ordinary
pendingCancel/stale-State defects were not reproduced, so no speculative ledger
or additional cleanup algorithm is proposed.

The later r612 final-owner controller gate FAILED overall (FD4->5 and final
signal EPERM); its14 inner events do not close cleanup or Arc-deallocation
uncertainty. It is not included as a passing result or as source in this packet.
Windows42e eight-case execution remains held for ROOT review. No native-error,
allocator, borrowed-held or old-concurrency campaign is restarted. Full
population/kernel return, cost and valid destroy-error proof remain ROOT-owned.
No missing producer or test result is replaced with a status attestation.

## R01–R16 and this packet's checks

| Rules | Evidence and limit |
| --- | --- |
| R01/R04 | Actual typed owned API correction and current-source red at33df precede fix; exceptional native errors, retained allocations and concurrent exceptional safety remain open. |
| R02/R03 | Existing pinned Rust/Apple/triomphe alternatives, measured cold need and original derivations retained in BINDING; no new architecture, runtime number or default. |
| R05/R06/R07/R12/R13 | Actual State/Tail/Child seam, three owned files, unchanged borrowed span; no new framework, manager, cache, warming/polling/spin, OS duplication or deliverable in this closeout. |
| R08/R14 | No performance/allocation-free/leak-free or parent/kernel resource claim; ROOT owns profiles and costs. |
| R09/R10 | Exact pre-fix red, original guard removal/restoration and source-specific native proof retained; no historic result relabelled as a new main pass or execution. |
| R11 | Exact native GNU/Darwin/Windows results and cfg/ignore/musl limits above; current combined seven targets/VMs still owed. Intel Mac unsupported, WinARM paused. |
| R15/R16 | Evidence-only sole writer, no children/goals/actors/main merge, no target/cache/data/source/frozen-artifact cleanup. This packet does not authorize any held campaign. |

This batch checks only exact source identities, patch syntax and document/policy
mechanics. `git apply --numstat` returns0 and parses exactly three paths with
726 added/125 removed lines. `python3 scripts/check-development-policy` returns0.
Full staged whitespace checking returns2 for eleven authentic blank context
markers in source.patch, preserved verbatim; README/manifest-only checking
returns0. The manifest retains exact argv/stdout/status. No compiler, Rust test,
native process fixture or runtime correctness check is executed.
