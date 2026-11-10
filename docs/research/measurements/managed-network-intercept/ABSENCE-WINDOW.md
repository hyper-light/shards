# Bounded relay-absence observation

Status: **experiment-only predicate; actual new-source guests/mutations unrun by
this lane**. Signed source `2a173a2ea805a1f64c7dd85e0b217fc97365fd4f`, pushed branch
`p/managed-network-absence-window`, separate worktree from preserved
`e27cd2aa85e43e66eca53f0eda18d50233bf9736`. Only intercept.rs and the existing
host managed_network fixture change source. Wire is byte-identical to eb0,
SHA256 `d3153a5fcbdecbd990b5ff6c2d3fb5e6dbefc4b6ba8a87d7b372e16514609d1c`.
No guard, timer, deadline, clock, count, budget, kernel/config or product change.

POLICY_READ implementer: full current g121-main AGENTS/R01–R16/evidence/CLAUDE/
managed plan/current handoff; revision `f5cec365d2e435afae5e6f613c92d9d3759ef60f`,
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Owned scope is these two fixture files/pure controls and experiment docs. Root
owns all audits/profiling/benchmarks/heavy guests/chaos/sweeps, source review,
combined gates, guard mutations/restoration and adoption. No children/main merge.

## Actual coordinator data and narrow decision

Coordinator r284 at exact `2cc8ddcd8b7b2cd818c09f1864c8a21aa2c6b7f3` completed
its driver0: all four present variants0; guarded absence still returned strict
cargo101 with ClientOutcome ConnectCall/Deadline/TimedOut, Joined backend
accepted1/receipts1/terminalDeadline, relay0/joinedtrue, WorkerError TimedOut and
the old Inconclusive verdict. Root reports no new backend effect during that
original window, raw `~/managed-intercept-drop-2cc8-v1`. Earlier r279 at37a had
actual no-guard BACKEND success and backend2/receipts2, raw
`~/managed-combined-37a6-runtime-v1`. These are coordinator-supplied earlier-source
results, not this patch's passing gates. Original raw/inputs remain root-owned.

Root explicitly authorized a distinct bounded verdict after these observations.
Treating every error as containment would admit setup/postconnect/worker failures.
Calling the deadline a refusal would falsify the actual outcome. Increasing the
wait, adding a health probe, changing the guard/kernel or installing REJECT only
to obtain refusal is outside this scope. The primary Linux DROP/BREAK/immediate/
REJECT comparison and pinned hashes remain [ABSENT-DROP.md](ABSENT-DROP.md) and
[absent-drop-primary-sources.json](absent-drop-primary-sources.json). This change
represents the observed terminal state at the existing seam; it neither selects a
production mechanism nor changes packet evaluation.

## Exact accepted and rejected facts

| Client/owned backend facts after existing join | Verdict and result |
| --- | --- |
| Successful exact BACKEND response, any second backend accept OR exact SENT+EOF receipt, regardless of other errors | DirectBackendEffect, nonzero. |
| ConnectCall/Refused/ConnectionRefused; baseline1 accept AND1 receipt; backend Stopped | Existing ContainedRefusal, checked AttemptUntilJoin window. |
| ConnectCall/Deadline/TimedOut; baseline1 accept AND1 receipt; backend known Stopped OR Deadline | New NoNewBackendEffectBeforeDeadline, checked OriginalAbsoluteDeadline window. |
| Missing/unknown baseline counts, preconnect/postconnect failure, other client error or backend IO error | Inconclusive/nonzero. |
| Unknown join/panicked worker | Existing completed? boundary returns nonzero before an accepted verdict or checked record; no counts are invented. |

Both eligible branches also require the real flow's relay listener closed before
the fresh forward call, relay worker NotStarted/0 accepts/0 receipts and a known
joined backend. The baseline call must already have exchanged exact SENT/BACKEND
bytes successfully. The host independently requires one actual baseline backend
ServerObservation, its local tuple equal to baseline destination and its peer
equal to the baseline client's actual local tuple, with exactly one baseline
ClientObservation and no Relay receipt. These are actual observations, not a
synthetic presence flag. The classifier keeps its original &Completion seam;
unknown joins remain at the real fallible finish boundary, with no optional
completion API introduced solely for a pure test.

Client stage/outcome/kind/errno and backend terminal/counts remain visible. The
existing WorkerError TimedOut record is emitted before classification. Only the
new exact deadline verdict permits that expected worker TimedOut to stop being
fatal; every other worker error still propagates. Direct-effect classification
runs first and its record remains decisive even when another IO error propagates.
An immediate refusal paired with backend Deadline is not silently broadened into
the new case; its existing stricter refusal predicate remains separate.

RelayAbsentChecked now repeats the typed verdict and typed AbsenceWindow and
explicitly emits after_window=Unverified. The host matches each branch's client
kind/outcome and backend terminal. Deadline completion requires exactly one
Backend/TimedOut WorkerError; Stopped requires none. It checks verdict/window/
limitation consistency and both independent baseline counts before status0.
OriginalAbsoluteDeadline names the original caller-bound observation, with no
new duration or clock. It makes **no promise about packets or effects after that
window**, forever-denial, complete mediation, other flows or resource return.

## Actual light checks, freshness and preserved records

Final private Fedora r88/v2 passed **7/7 pure tests**, strict amd64 GNU clippy0
and musl clippy0 using Rust1.98.1. Pure controls include deadline with backend
Stopped and Deadline, missing/partial/unknown baseline counts, backend other IO
error, preconnect/postconnect errors, and direct accept/receipt/success dominance.
Existing refusal and exact guard/selector/ACK vectors remain. Unknown join is
handled at the unchanged real boundary, not claimed as an injected runtime test.
No sockets, namespaces, VM, fault/mutation or profile was run here. Host changes
were format/syntax checked only; host compilation/execution is owed to root.

Same exact4CPU/8GiB/20GiB admission; private sync directory
`~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-network-absence-window-a26234000a`,
this lane's own `~/managed-network-relay-absent-target`. Owned package clean and
source touch force freshness after sync; actual compiler logs/source hashes are
retained. No other lane/root clone/target write. Final frozen unit binary SHA256
`2c558a7322a8fa9e41eb833952877ec4b68c5a65b680ae15ff7ad395c2716449` at
`~/managed-network-absence-window-proof/v2/shards_testguest-975e61748c61ff31`.

[absence-window-light.tar.gz](absence-window-light.tar.gz), SHA256
`fa54137217d5cdb43312f7c1d62521e6a8c0eb625751756b15317f07f2121a6f`, contains26
verified raw command/log/exit/compiler/source/binary records and an internal
manifest, expanded at `/Users/adalundhe/g121-managed-network-absence-window-light`.
It preserves r87/v1's earlier seven-pure/GNU/musl0 checks separately from final
r88/v2. V1's optional completion/presence experiment was removed during review;
its source remains `/Users/adalundhe/g121-managed-network-absence-window-v1-source`
and binary proof/v1. Those intermediate passes are not final gates. Final source
identities and unchanged wire/driver checks: [absence-window-sources.json](absence-window-sources.json).

The unchanged [run-light.sh](run-light.sh) used the above target,
RECORDS=`$HOME/managed-network-absence-window-light-v2`, musl CC/AR=this checkout's
scripts/zig-cc and zig-ar, ZIG_TARGET=x86_64-linux-musl. Each exact cargo command
and exit is retained. [Local static checks](absence-window-local-checks.txt)
(format/shell/whitespace/policy) all0; no local compiler/Cargo/VM invocation.
Observed local free39880068KiB exceeds30GiB. No deletion/headroom bypass or lane
job remains active. Both prior source checkpoints and their frozen evidence stay
preserved.

## Owed coordinator execution and completion limit

Review exact diff e27..2a173a2ea for the two owned fixture paths. Root then owes
host compile/parser, actual guarded bounded-verdict guest, all four present
regressions, independent removal of **only the appended DROP rule** with actual
backend2/receipts2/BACKEND/direct-effect red, byte-identical restored bounded green,
and ownership/failure/unmatched/resource/cost/target gates. This lane has not run
or passed them. Root explicitly requires those effect/mutation/restoration facts
before calling this bounded fixture complete; broader P4/managed-runtime gaps
remain open regardless.

With the same reviewed absolute experimental kernel/effective config/provenance,
explicit SHARDS_NETWORK_BOOT_MS/CASE_MS/MEMORY_MIB/CPUS and mark/table/priority,
private target and unchanged4CPU/8GiB/20GiB admission:

```sh
export SHARDS_INTERCEPT_ORIGIN=forward-relay-absent
export SHARDS_NETWORK_RECORDS="$HOME/managed-intercept-reviewed-absence-window"
bash docs/research/measurements/managed-network-intercept/run.sh
```

This is the exact owed entry, not an executed result. Kernel/config identities
and other required inputs remain [RELAY-ABSENT.md](RELAY-ABSENT.md#owed-coordinator-execution).

## R01–R16 record

| Rule | Scoped evidence and remaining gap |
| --- | --- |
| R01 | Result handling and exhaustive typed matches; strict GNU/musl0. No whole-path std/internal allocation/panic/fault certification. |
| R02 | Root r284/r279 actual data and pinned prior primary/kernel alternatives; narrow authorized outcome representation, no adoption/cost inference. |
| R03 | Original deadline/count authority and wire unchanged; no numeric policy or timer copied from measurement. |
| R04 | New explicit verdict/window, preserved client/worker cause/errno and unknown-join boundary; host matches exact cases and limitation. |
| R05 | Existing flow, eventfd/deadline/join seam; original completion interface, no cache/polling/new manager. |
| R06 | Focused classifier/match and bounded pure cases; no topology/socket-engine expansion. |
| R07 | Comment explains exactly when expected backend deadline stops being fatal; facts/limits documented. |
| R08 | No benchmark/4–5x claim; target unmeasured. |
| R09 | Coordinator's exact live guarded outcome preceded predicate change; no invented new-source guest pass or product bug. |
| R10 | Signed source, exact raw commands/logs/exits, source/frozen artifacts and prior prototypes retained; real final/mutation/restored/cleanup proof owed. |
| R11 | GNU unit/compiler and musl compiler only; host compile, remaining seven-target/hardware/runtime gates root-owned and unverified here. macOS arm64 only. |
| R12 | Shared existing fixture seam; no extra installed binary, host root, guard/kernel/config/pin/crypto or product change. |
| R13 | Bounded observation semantics only; later effects and full mediation not asserted. |
| R14 | No profile/benchmark/allocation/fault/CPU/resource-return claim; root owns all such campaigns. |
| R15 | Current f5/d634 full implementer receipt, scoped light checks, signed named source first; no children/main merge. |
| R16 | Own private clone/target and preserved v1/v2/raw/source, exact admission, local floor, no root-cache write/deletion/bypass/local build. |
