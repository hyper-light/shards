# Conditional terminal completion/stop ordering

**Proposal obligations only. No production change or reproduced metadata defect
is claimed.** ROOT's r632/r662 observed member1's application ACKs and transport
EOF after the separately fixed finish-wake Full cause. Later fixture failures,
most recently duplicate ACK Full in its observer, still prevent the final strict
metadata result. The tenant lane owns that fixture correction. Header-only
intermediate metadata does not authorize e1cb or close the complete normal case.

POLICY_READ IMPLEMENTER: full canonical AGENTS.md, R01–R16, evidence checklist,
CLAUDE.md, managed-runtime-plan.md and `/Users/adalundhe/g121-handoff.md` read;
current main `69dbc3180b8c8c55cb03f9f95f822d2752ba2887`, unchanged policyfiles
f5cec365/rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Coordinator `01a148884a4770fdaf0111f3f792a248`, implementer
`01a1c5d2ee5871a2a9a9cdc9f56793f0`. Own only this new document/evidence directory,
using the existing clean worktree on `p/managed-terminal-linearization`.
No children, goal-plan expansions, automatic delegation or goal resets: ROOT
alone assigns actors. No local build, new full worktree, protected target write,
VM/socket/profile/campaign or source implementation runs here.

The earlier conditional proposal is preserved at signed
`e1cb83139551b404736bc036277ccdb536ad409d:docs/research/managed-terminal-proposal.md`.
This supplement tightens its ordering obligations against current main, after
the distinct queue-wake fix. It chooses no new API, terminal format or arbitration
rule ahead of ROOT's actual full-case evidence.

## Keep the three authoritative facts separate

The canonical stream supplies End or Aborted(errno). Remote Shared supplies the
exact encoded-wire acceptance/consumption and Finished/Aborted state. Output Life
owns the logical capture outcome. Equality of accepted/consumed alone cannot
replace either terminal cause or a typed Finished observation. Canonical payload
offset, application ACK cursor, transport FIN, process reap and metadata durability
remain separate facts; see [RFC9000 sections3.1–3.2 and4.5](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.1).

If ROOT authorizes a later implementation, its proposed transition contract must
distinguish at least these cases, without a copied progress/terminal cache:

| Canonical/logical fact | Remote fact | Obligation, not a claimed current result |
| --- | --- | --- |
| Draining a canonical End | Final record application-ACKed, typed Finished | Eligible for one normal logical terminal publication; FIN delivery remains independently checked. |
| Draining canonical Aborted(errno) | The same wire delivery completes | Retain the canonical failure/errno; fully consumed wire bytes do not turn the capture into normal End. |
| Draining either cause | Pending or typed Aborted, even with equal cursors | Preserve uncertainty and the known cause; do not infer clean completion from counters. |
| Life already terminal | Duplicate completion, endpoint return, observer cancel or owner stop | Logical terminal remains sticky, while owned transport stop/retirement still happens. |

Current Output.abort's terminal early return was written when Ended followed
endpoint.run return. Publishing Ended earlier would create a new still-owned
transport path. A future change must explicitly separate logical publication
from unconditional owned transport stop; this is not a claim that the old path
leaks a running endpoint.

## Inline notification and the wait predicate

Current Output.finish holds Life while Control.finish invokes Shared.notify;
Shared.notify calls its registered callback inline after releasing Shared.
Therefore the callback must remain wake-only: no Life acquisition, metadata I/O,
ACK waiting or endpoint joining. Completion processing belongs in the existing
credit worker after releasing credit/Shared locks, with explicit draining work
rather than an immediately actionable loop or a new worker/manager.

For a candidate credit-worker wait, synchronize the completion predicate with
the existing credit mutex. One proposed ordering to prove is: hold credit while
observing Shared; if pending, enter its existing condvar wait; Shared publication
releases Shared before its callback coordinates with credit and notifies. If
publication precedes the predicate read it is observed; if it follows, the
callback cannot race past the atomic release/wait boundary. Recheck after every
wake and preserve the existing poison-tolerant handling and single-mutex use.
This is a proposed proof obligation, not a reproduced lost wake.

The lock graph must not gain a held credit→Life edge. Querying Shared while
credit is held needs a proof that publication never holds Shared while acquiring
credit. A worker must release those guards before claiming the Life transition
or writing metadata. New notifications must also wake draining work when producer
capacity is zero; capacity is not itself a terminal predicate. Any relocation of
the existing learning observation must preserve its sample meaning, with cost
claims remaining ROOT-owned/unmeasured.

## Completion racing owner stop: required arbitration proof

The ordinary final-metadata example is insufficient to prove every concurrent
ordering. The required contract cannot let worker scheduling erase an already
known normal completion. If the typed Finished observation for canonical End
precedes owner stop, an eventual stop must preserve that outcome while stopping
transport, even if the credit worker has not yet published Life. If revocation
linearizes before that completion, its distinct uncertainty/cause remains.

A candidate therefore needs one consistent resolution through the existing Life
owner before a stop overwrites relevant Shared state. Linearizing solely at a
delayed worker publication would permit an already-finished End to be relabeled
aborted/unknown; that alternative does not meet the required promise. No method
or fix is implemented here. Keep End versus Aborted(errno), known wire progress
and sticky final cause, and stop the owned endpoint after either winner. A stop
preceding the ACK and one following it need distinct retained witnesses.
Snapshotting counters outside the chosen ordering is not a consistency proof or
an extra authority source.

Current metadata writes ignore their result. Any future changed publication path
must handle/report write failure explicitly and keep owned stop progressing;
successful application consumption is not proof that a line was appended/synced.
The metadata error's scope/presentation and retention guarantee need the concrete
contract and ROOT evidence; this proposal adds no new fallback, format or error
ledger to guess those semantics.

## Checked source support and unrun gates

Kiln research `01a1c5d333de754580d8ac130a9b3bdd` records these **inferences**, not
live defects. Grades describe cited support, not a probability of correctness:

- **C ·68 (moderate), f5:** A future credit-worker drain wait must check its
  completion predicate while coordinating with the same existing credit mutex;
  Condvar wait atomically unlocks, may wake spuriously, and supplies no evidence
  of remote completion by itself. One independent official source; both quotes
  found. [Rust Condvar::wait](https://doc.rust-lang.org/std/sync/struct.Condvar.html#method.wait).
- **B ·79 (good), f6:** The existing synchronous Shared.notify callback must not
  reacquire Output Life during a future terminal-publication change: Output.finish
  holds Life while calling Control.finish, whose notify invokes the callback
  inline. Two independent code/docs origins; all three quotes found. Sources:
  current main69db `remote/src/managed.rs::Shared::notify`,
  `shards/src/daemon/managed.rs::Output::finish`, and
  [Rust Mutex::lock](https://doc.rust-lang.org/std/sync/struct.Mutex.html#method.lock).

Source reads are limited to those existing seams and their concrete credit
caller. ROOT still owes the full normal fixture, separate write/stop/completion
order witnesses and any guard mutations/fault/actual VM validation. No assertion
is relaxed to admit aborted/unknown for a required normal complete tail. The
required plan is not reclassified as complete and this lane is not released.

[policy-result.json](policy-result.json) retains an actual mechanical policy
check, exit0, in new private Fedora source
`/home/adalundhe/managed-terminal-linearization-policy-69db-v1` with
1 CPU/1 GiB/2 GiB admission. No build/unit/native workload ran. Passing that
check supplies no liveness, metadata, race, performance or merge evidence.

R01/R04/R06/R07/R13 remain implementation obligations; no production code changes.
R02/R03/R05/R12: primary sources and explicit alternatives/gaps, no selected new
runtime literal, API, wait/cache/manager/thread or OS adapter. R09/R10 require the
still-pending exact ROOT metadata reproduction before a fix. R08/R14 all costs,
profiles/allocations/faults/comparisons unrun. R11 no platform or hardware proof
from this prose, macOS arm64 only and Windows ARM paused. R15/R16 keep ROOT's
audits/campaigns/main merge, preserve every checkpoint/source/evidence/private
target, and create no local build or actor/session.
