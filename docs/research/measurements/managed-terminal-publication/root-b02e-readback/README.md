# ROOT terminal result readback, b02e

This is an implementation-owned readback of ROOT's retained before/after data.
ROOT ran every actual VM/caller/unit campaign named here. This lane copied and
hashed74 selected text records read-only; it did not rerun those tests, mutate
ROOT's clone/target, run a profile or own the master coordinator archive. The
exact read command is [read-root-records.py](read-root-records.py); its Python
payload was retained after execution under the outer `python3 - <<'PY' ... PY`.
[raw/MANIFEST.json](raw/MANIFEST.json) gives original paths and member hashes.

POLICY_READ IMPLEMENTER `01a1c5d2ee5871a2a9a9cdc9f56793f0`, coordinator
`01a148884a4770fdaf0111f3f792a248`. FULL canonical AGENTS.md, development-rules
R01–R16, development-evidence.md, CLAUDE.md, managed-runtime-plan.md and current
`/Users/adalundhe/g121-handoff.md` reread on continuation at canonical
`eb5654d6f5356cacaca536ff51f14a632fa7e801`; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`, policyfiles
f5cec365 unchanged. Receipt `refactor-policy-resume-eb565-goal3-v1` preceded
dependent work. Own only this evidence subdirectory, new own branch
`p/managed-terminal-root-readback` from frozen d260 in the existing worktree.
No production source changes. d260 and the separate operator24fac9fee source
remain preserved; no children, goal-plan expansion, main merge or local build.

## Exact sources and unchanged contracts

ROOT's clean baseline was
`26bab0778ddb8f35b11e4b5644ee83b1b1d0df99`, with neither temporary product
diagnostics nor the terminal fix. It composes the normal and stdio proof fixtures
onto maineb5654. ROOT's after source is
`b02e8592c547553c788702ac6c52ea6b633e875d`, composing only the reviewed
`d26084720cb86e530bf9f9b69af91984b6c85539` terminal change.

[source-identity.json](source-identity.json) records byte equality of the two
production files against d260 and unchanged before/after hashes of the full
managed-output test parent, normal/stdio fixture files and testguest main.
Recorded inputs independently name the correct before/after source revisions.
The original ended/complete assertion is unchanged; no fixture assertion was
relaxed, skipped or replaced with a driver-level success condition.

| Production file | Actual b02e/d260 SHA256 |
| --- | --- |
| daemon/managed.rs | `2b3e4d2c80be615a58b062b6a077c11511e2679c738c798a27f0e014a880f655` |
| remote/managed.rs | `79c949b781e71821079a0858c67270355ed24a231789aafad80c9695f6226fdd` |

ROOT's earlier eaf0/r668 exact metadata red is already retained on main4bfaceb38
and linked by [the implementation record](../README.md). The clean26ba baseline
below strengthens that same defect's reproduction; it is not a new bug category.

## Before and after actual cases

| ROOT invocation | Before r692,26ba | After r696,b02e |
| --- | --- | --- |
| `tenant_exit::member_normal_exit_preserves_sibling_and_main_progress --exact --ignored --nocapture --test-threads=1` | One test, strict metadata failure101 | One passed, zero failed, exit0 |
| `tenant_exit::stdio::actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime --exact --ignored --nocapture --test-threads=1` | One test, strict metadata failure101 | One passed, zero failed, exit0 |

The exact test binary path/arguments, full logs and exits are retained under
`raw/{before,after}/{normal,stdio}/`. After source/compile/driver exits and binary
artifact identity are under `raw/after/`. These are actual one-case results,
not zero-filter matches, expected101 relabeled as pass, or `protocol_complete`
alone. ROOT's r696 completed driver.exit0 after all its selected checks.

Before, both cases reached canonical member1 End/application ACK/transport EOF,
later sibling stdout/stderr and independent main progress, then successful
stop/remove/owner/observer cleanup with FD7→7. Intermediate metadata was
header-only. Stopped member1 rows were aborted/unknown at102/102 and99/99.
The continuing sibling was aborted/unknown at177/177 and171/171, as expected.

After, both member1 rows already appear in the immediate member-ended/actor-eof
snapshot and the before-stop snapshot:

```text
member 1 channel 1 state ended accepted 102 consumed 102 tail complete
member 1 channel 2 state ended accepted 99 consumed 99 tail complete
```

They remain exactly once in stopped metadata. Continuing sibling rows remain
aborted/unknown at177/177 and171/171. Before/after controlled neighbor-progress
JSON is byte-identical for each corresponding case. Actual stop/remove and
inspect false0, owner stop/observer join and observerFD7→7 are retained. Full
population reaping and kernel resource return are still explicitly Unverified
in the original teardown records; these counts do not establish leak freedom.

The stdio case additionally records a fresh challenge sent after both output
EOFs, the exact echo/peer EOF, a visible live actor UID row, new sibling fd1/fd2
and independent main progress. That ordering passes before the final metadata
assertion in both revisions. The fix preserves channel/lifecycle separation:
ended/complete is evidence of canonical output consumption, not actor exit or
population reap. [After actor-control](raw/after/stdio/records/actor-control.json)
and [subsequent progress](raw/after/stdio/records/subsequent-progress.json) retain
the raw controlled witness, rather than infer it from the word Ended.

## Actual full caller/unit and reported platform gates

ROOT r696's full retained commands/logs/exits confirm:

- ordinary managed-output caller:16 passed,4 ignored,0 failed, exit0;
- actual complete `daemon::managed::` selector:15 passed,0 failed, exit0;
- actual complete remote `managed::tests` selector:15 passed,0 failed, exit0.

These are real full-crate tests, beyond the agent's original exact-fragment/
call-recorder checks. The ignored tests remain ignored in the ordinary invocation;
the two strict VM cases were run separately as above. The frozen d260 agent
record still correctly describes its14 fragment/call plus3 real Shared checks;
ROOT's later results do not turn those earlier tests into something broader.

ROOT's direct message on this continuation also confirms r697 on Macb24 source
b02e: full seven-target scripts/lint exit0 plus15/15 native Mac remote tests.
That result is coordinator-reported here; this74-member selection does not
contain the r697 raw archive. Windows ARM remains paused; seven-target lint and
native Mac tests do not imply all supported VM combinations ran.

## Remaining authority and validation

ROOT r702 is active, exclusively owning its source/cache. It disables only the
canonical End+Finished→Ended arm and requires actual raw canonical End plus the
corresponding false aborted metadata and strict101, then exact daemon hash2b3e4d
and empty-diff restoration followed by fresh normal/stdio0. No result from that
campaign is asserted here. This lane neither starts nor repeats it.

Publication ordering with slow/failing metadata, completion/stop race witnesses,
canonical Aborted(errno), withheld ACK, disconnect/reconnect/revoke and actual
resource/mutation validation still belong to ROOT as listed in the original
implementation record. A pair of normal greens is not a full race proof,
durability guarantee, performance result or closure of P0–P13/G01–G24. No
additional production fix follows from these passing results or source guesses.

For the bounded implementation-return objective: exact ROOT red and explicit
authorization preceded source edits; canonical/error/stop/notify semantics and
targeted checks were returned in signed d260; the primary-source alternatives,
before evidence and owed ROOT gates are retained; actual before/after caller
evidence now confirms the unchanged strict normal/stdio contract. The overall
lane/managed-runtime goal remains active until ROOT confirms closure and releases
it. Operator24fa/c2 and Poller d8aa remain separate, optional, preserved work.

R01/R04/R06/R07/R13: no source change; original implementation/pure proof plus
actual ROOT full selectors are distinguished above. R02/R03/R05/R12: previous
researched existing seam remains, no new choice/constant/wait/cache/adapter.
R08/R14: no profile, distribution, comparative or allocation/fault claim.
R09/R10: exact clean red/green identities, unchanged fixtures, original raw data
and per-member hashes retained, meaningful mutation still pending. R11: actual
GNU VM/caller/unit evidence and separately reported seven-target/native Mac
scope, remaining hardware unverified. R15: readback only, all campaigns/merge/
closure authority remain ROOT; no children/automatic dispatch. R16: tiny text
record copies/private policy check only, no build/local target/cache/deletion;
protected ROOT source/targets and paused VM untouched.
