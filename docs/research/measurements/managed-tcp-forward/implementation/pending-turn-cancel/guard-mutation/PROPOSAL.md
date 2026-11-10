# Single pending-cancel admission guard: prepared, not executed

This is an unapplied code/protocol proposal for ROOT review. No compilation,
native case, mutation or VM has run for this proposal. The actual four-case
[a9f/8de proof](../VALIDATION.md), its original source, raw records, failures and
frozen binaries remain unchanged. No production defect or correction is claimed.

POLICY_READ current main0342b85d9c808e3ced041a6c3ef0e9bf70438272, rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3; implementer
and assigned adversarial relay tester. The full policy/evidence/CLAUDE/plan/handoff
and required closing records remain loaded from the unchanged context. Preparation
receipt7041 owns only the four NEW files in this directory. ROOT owns review,
combined integration, audits/profiles/benchmarks and main merges. No children or
goal wrappers. Paused WinARM, held campaigns and ROOT paths remain untouched.

Preparation checks: both unapplied patches pass `git apply --check`; mechanical
development policy passes. These checks execute no proposed Rust code. A read of
the old `turn-pressure/NEXT.md` at the current checkout path returned1 because
that file was deliberately not carried onto0342; its original df7 Git blob was
then read successfully. This source lookup miss is not a native test outcome.
Actual local free at this preparation check was179558420KiB, above30GiB. No local
build/target/cache or prospective remote path was created.

## Exact proposed changes

[source-identities.json](source-identities.json) retains the complete tested8de
14-file closure and the two unapplied patches' preimage/postimage identities.
The native driver remains byte-identical e8c06dd2a… with its strict assertions.

1. [observe-complete-suffix.patch](observe-complete-suffix.patch) changes only
   cfg(test) `turn_native.rs`. Its original independent one-byte backend probe
   becomes a finite sequence of real one-byte reads, stopping on actual EOF or
   error. The maximum calls are the already-observed pending length plus the
   existing one-byte probe size: at most4 for this fixture's real pending3.
   Every continuation retains a newly read byte. The bytes reuse `received`,
   whose prefill was already independently logged and checked; no buffer, queue,
   helper, thread, actor or deadline is added. An explicit bounds check precedes
   these reads. Full actual bytes are recorded before the existing strict checks.
   A final positive read or an error remains a failure, never inferred EOF.
2. [remove-one-admission-guard.patch](remove-one-admission-guard.patch) deletes
   exactly this ONE statement in the private copy of shared `Deadline::io`,
   immediately after the original post-readiness expiry check:

   ```diff
                    self.left()?;
   -                Self::cancellation_event(ready.revents)?;
                }
                match operation() {
   ```

The zero-time readiness syscall, negative-FD/native-poll error handling, both
original expiry checks, EINTR path, wait-path cancellation classification and
`Direction::turn` remain unchanged. The intentionally omitted statement is the
pre-operation classification of the ready cancel descriptor, including its
failure flags. It is a disposable test mutation, never a product change.
Cancellation stays `Some(SAME actual eventfd)` on every forwarding attempt;
the eventfd is not consumed. No wait timer, new allowance or picked number exists.

The observation patch is common to baseline, mutant and restoration. It is NOT
part of the mutated guard. New native binaries cannot inherit the original8de
passing result: their observer postimage must first compile and pass live.

## Observation and cleanup order

All order below is required, not an account of an executed mutation.

1. Before actors, record the immutable8de source closure, observer postimage,
   exact guard state, inputs, compiler/linker/libc, binary identities and actual
   admission receipt. Freeze each built binary outside target. Refuse any
   unexpected source diff or occupied output path.
2. Use the unchanged finite prefill input and real sockets. Stop at the FIRST
   actual native send-WouldBlock; separately retain accepted prefill bytes.
   The SAME Direction obtains pending from actual source recv. Its next real
   send-WouldBlock must preserve suffix/cursors/counters, as in the current test.
   Independently drain and record only the actual prefill before cancellation.
3. Record the actual8-byte write of value1 to the SAME owned eventfd, and positive
   original remaining time. No eventfd read, fake readiness or replacement at.
4. Invoke exactly the existing recovery `Direction::turn` once. Record its typed
   outcome, actual suffix and all counters before judging it. With the guard
   deleted, source analysis predicts that this turn can send the old pending
   suffix524551, then receive another source chunk in the SAME turn. Consequently
   its final in-memory suffix may differ from the forbidden bytes. Those state
   observations are not an independent backend receipt or a native guarantee.
5. Close the owned output. The independent backend then uses its existing trusted
   original observer Deadline(cancel=None, SAME absolute at). Retain each actual
   received byte in existing storage, then emit one complete
   `CancelledBackendProbe` byte receipt and the actual final read/error in
   `NativeTurnCancelTerminal`, before strict error/state/no-effect assertions.
   For the intended red, three positive one-byte reads plus an actual EOF0 would
   independently record524551. Missing/partial bytes, timeout, non-EOF or error
   are retained outcomes, not permission to tune, retry or infer the effect.
6. Return from `exercise` so owned streams, eventfd, slots and Direction drop.
   Record actual child FD before/after, positive original remaining time, zero
   additional workers and typed failure/cause BEFORE the existing outer asserts.
   The mutated native test should fail101 only AFTER these receipts.
7. The unchanged private parent holds the child's pidfd, observes exit readiness,
   directly waits/reaps it, closes its owned handles and records parent FD return
   and before/after binary hashes. Preserve stdout, stderr, observed.json and
   strict result.json even for the intentional negative. Forced cleanup targets
   only the owned child and disqualifies the intended witness.
8. In `finally`, restore the exact original relay bytes/hash, preserve a copy of
   the mutant source/diff, and verify the full restored closure before rebuilding
   and running controls. Relay-only diff must be empty against8de; the full
   checkout still has exactly the reviewed observer delta and declared native
   projection root. Do not falsely call that entire checkout diff empty.

## Proposed finite GNU-only sequence after ROOT review

One fresh admitted private scope on runner8fadc1ec213332e16cb0d915a3fb913f:
`/home/adalundhe/relay-turn-pending-cancel-guard-0342-v1/`
with `source/check/target/cargo-home/record/artifacts/cases`. These paths have not
been created. Linux GNU x86_64 only, 2CPU/2GiB/4GiB, total<=600s/-j2, unchanged
case30000ms/parent45000ms. Every actor must fit the remaining ORIGINAL outer
deadline; insufficient remaining time is a retained harness outcome. No local
target/cache/build, no extra runner permission or headroom bypass. Admission and
AS bounds do not establish physical/kernel hard caps.

| Exact source | Native cases, once each | Required classification |
| --- | --- | --- |
| Guarded8de + observer patch | pending-cancel, forwarding | Native0/driver0 each; full typed retention/EOF0, ordinary exact prefill+13-byte forwarding/EOF |
| SAME observer + ONE guard deletion | pending-cancel, forwarding | Cancel native101/strict driver1 AND actual independent forbidden bytes below; ordinary forwarding native0/driver0 |
| Byte-exact restored guarded closure | pending-cancel, forwarding | Native0/driver0 each, guarded retention/EOF0 and ordinary exact forwarding |

Keep the current driver/filter/input bytes. The mutant's strict red is never
relabelled a passing ordinary test. A separate campaign classification must
require ALL of these raw facts, not merely native101 or driver1:

- Real original prefill/WouldBlock/source-recv/blocked-send witnesses unchanged.
- Actual8-byte signal to the same eventfd before the attempted turn, original
  remaining time positive, and no successful forwarding before that signal.
- Nonempty independent `CancelledBackendProbe.hex` equals the original actual
  `BlockedSend.suffix_hex` (524551/3 with these inputs), followed by actual EOF0
  and no read error. No production counter or wrong error text substitutes.
- Probe/terminal and cleanup receipts precede strict assertions; child/parent FD
  return, pidfd exit readiness/direct reap, no forced cleanup, exact before/after
  binary/source identities and restored controls all satisfy their checks.

Only a concrete ROOT review authorizes the proposed execution. This packet does
not authorize new mutations, retries, alternate pressure inputs, VM, profiles,
seven-target or combined sweeps. A native coverage miss stays a miss.

## R01–R16 preparation record and remaining gates

| Rule | Preparation evidence / remaining limit |
| --- | --- |
| R01 | Unapplied cfg(test) observer only; existing strict production code untouched. Compilation/lint/native remain unrun for these postimages. |
| R02 | Actual8de/ce601 pressure/cancel data and inspected shared Deadline/attempt/Direction call chain; no architectural choice. |
| R03 | Same explicit caller bytes/queues/deadlines; finite observer calls derive from actual pending plus existing one-byte terminal probe. No production number. |
| R04 | Existing typed Cancelled/Expired/native fault and half/progress facts remain; actual final read distinguishes EOF/error/bytes. |
| R05 | Same authoritative Direction/Deadline/eventfd, original observer, existing storage; no new manager/cache/queue/polling scheduler. |
| R06 | One guard statement omitted; one test observer extended without a helper or production visibility change. |
| R07 | Patch comments explain storage reuse, progress and independent terminal proof. |
| R08 | No performance comparison or4–5x claim. |
| R09 | No production fix; proposed guard mutation requires actual independent effect before any conclusion. |
| R10 | Exact unapplied patch bytes/hashes; all original proof retained. New before/mutant/restored raw is not yet produced. |
| R11 | Future GNU-native x86_64 only. Prior musl/VSOCK/matrix gates stay revision-specific; no new platform result. Mac arm64 only. |
| R12 | Same shared pump and native seam; no OS adapter, parallel implementation or installed binary. |
| R13 | Proposal changes only the observer receipt and a private one-line mutation. |
| R14 | No profiling/benchmark/audit; parent/kernel costs, allocations and leak freedom unverified. |
| R15 | Preparation only pending ROOT concrete review. Exact owned source/runner/bounds/cleanup and six finite cases above; no children/goals/main merge. |
| R16 | No build/cache/target/large copy or cleanup; all prior frozen sources/raw/failures/artifacts preserved. |

ROOT review and any later assigned native execution are owed. Combined selected
source integration remains ROOT work. Existing0342 carried VSOCK/matrix adoption
is not rerun or inherited here. Checked-ready/concurrent-cancel races, full route
coverage, hard parent/kernel entitlement and management P0–P13/G25 remain open.
