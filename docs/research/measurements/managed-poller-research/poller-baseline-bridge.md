# Existing Poller baseline: root invocation and interpretation bridge

This connects the original approved Poller research/measurement proposal to the
refactor lane's now-existing baseline harness at exact commit
`f2a2ede508e3e860189c9611f9c6a1cd24ca4197`. It is a focused read of that
necessary fixture caller and its input/record wrapper, not an audit, benchmark,
production candidate or harness modification. **Nothing below was executed.**
Root owns complete-harness review/compilation, native descriptor work, failure
cases, profiling, comparisons and main acceptance. The original lane requested
the next literal and is closing this previously missing concrete invocation
requirement while root reviews the completed TLS/QUIC/ring research pieces.

POLICY_READ implementer; canonical R01–R16 SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`,
full managed plan at main7c26 SHA-256
`ebae0bb6922dba5f90d036d06310cc546352279fd368b5c307770a84ddc4a968`,
full handoff42f. The old harness branch cannot waive current policy. This lane
owns only this research subtree and the main note; it does not edit or run the
other lane's harness. [Identity record](poller-baseline-source-identity.json).

The [exact new Kiln findings/source excerpt](poller-baseline-kiln-research.md)
retains B/79 and B/72 with all five cited code quotes found. Automated grades
are not runtime evidence, semantic certification or a sizing criterion.

## Exact existing source and scope

The existing [README](https://github.com/hyper-light/shards/blob/f2a2ede508e3e860189c9611f9c6a1cd24ca4197/docs/research/measurements/managed-poller-cost/README.md),
[fixture](https://github.com/hyper-light/shards/blob/f2a2ede508e3e860189c9611f9c6a1cd24ca4197/docs/research/measurements/managed-poller-cost/fixture.rs),
[pure input/state helper](https://github.com/hyper-light/shards/blob/f2a2ede508e3e860189c9611f9c6a1cd24ca4197/docs/research/measurements/managed-poller-cost/accounting.rs)
and [root wrapper](https://github.com/hyper-light/shards/blob/f2a2ede508e3e860189c9611f9c6a1cd24ca4197/docs/research/measurements/managed-poller-cost/run.py)
have exact Git/blob/SHA-256 identities in this return. Its unix.rs and
platform/mod.rs blobs equal base24f and main7c26. That equality does not establish
the full fixture compiled or ran at any of those revisions.

The fixture creates real Unix socket pairs and one existing Poller per count
case. Each round writes one known byte to every held writer, waits/drains all
expected tokens and verifies WouldBlock after that byte while writers remain
open. One caller token Vec persists across waits, rounds and count cases.
Explicit removals, drop and baseline/acquired/released descriptor observations
occur before reporting. Partial setup/error also reaches cleanup. All native
behavior and those resource witnesses remain unrun by this lane.

All four positional fixture inputs are mandatory: ordered positive ready counts,
positive rounds, an invocation time budget in milliseconds and maximum wait
records. Repeated/decreasing counts are retained. That budget establishes one
absolute monotonic deadline covering setup and
all cases, rather than resetting after an empty/EINTR wait. Record/allocator/
descriptor/error/timeout failures make the invocation incomplete. Finite inputs
do not themselves provide hard process/kernel admission.

The harness contains no process-exit one-shot, empty-interest wait, concurrently
added/closed registration, sustained hot source, control thread or production
caller. It cannot establish those properties merely by completing a finite
socket round. Its reported five pure accounting tests were run by another lane;
this lane has only read that record. They are not native observations or a full
fixture compile result.

## Root-only command template, not a run

Root first admits source/target/runner/resources and reviews the complete
harness. Do not mutate any active root clone/target; user currently identifies
Fedora r644/source1533 as exclusive. A future run needs a clean, selected source
checkout and a new private work directory after ownership is released. This
lane creates neither. Only Linux GNU/musl on amd64/arm64 or macOS arm64 native
targets are in this Unix baseline; Windows and Intel Mac are outside it.

For the exact f2a source, the existing invocation is:

```sh
python3 "$ROOT_APPROVED_REPO/docs/research/measurements/managed-poller-cost/run.py" \
  "$ROOT_APPROVED_REPO" "$ROOT_APPROVED_NEW_WORK" \
  --revision f2a2ede508e3e860189c9611f9c6a1cd24ca4197 \
  --target "$ROOT_NATIVE_TARGET" \
  --ready-counts "$ROOT_ORDERED_COUNTS" --rounds "$ROOT_ROUNDS" \
  --deadline-ms "$ROOT_INVOCATION_BUDGET_MS" \
  --max-records "$ROOT_MAX_WAIT_RECORDS" \
  --command-timeout-seconds "$ROOT_COMMAND_TIMEOUT_SECONDS"
```

Variables are explicit root-selected admitted inputs; they have no defaults
here. Counts on both sides of the existing batch, plus repeated/decreasing
demand, exercise that baseline without selecting a new production batch.
Root supplies the finite case counts/deadline/record/resource envelope; this
document supplies no numeric workload recommendation.

The wrapper refuses mismatching HEAD, tracked changes, an existing work directory
or an out-of-scope target; builds offline with one Cargo job into its new target;
and records actual toolchain, source/lock/harness/binary identities, exact command
status and raw stdout/stderr. Offline missing dependencies are a refusal, not
permission to alter source/inputs or root caches. A merged-source run requires
that run's actual revision and fresh identity checks; old f2a results cannot be
relabeled a newer revision's gate.

The wrapper's per-command timeout and the fixture's one absolute native
invocation deadline are separate: build time is not in the latter. A command
timeout can kill the fixture during cleanup/reporting; it is incomplete and
does not establish descriptor/resource return. Root must retain that result and
independent cleanup evidence, not silently extend/reset an expired native case.

## Record-cap arithmetic is a lower bound, not a tuning rule

Let C be the existing native batch bound, R the caller's rounds and c_j the
positive count for case j. For a successful fixture invocation:

```text
necessary_wait_records >= R * sum_j ceil(c_j / C)
```

Each round must drain c_j distinct expected tokens, and one successful Poller
wait supplies at most C tokens. This proves the necessary inequality. It is not
sufficient: partial/empty successful waits consume additional records, and the
OS does not promise every returned batch is full. Use checked arithmetic; a
division/remainder ceiling avoids introducing `c_j + C - 1` overflow.

The existing pure parser only rejects a cap smaller than `R * number_of_cases`.
That is its declared input check, not a guarantee a native run can finish within
the cap. A parser test accepting an input is not a native successful-result
witness. This is a source-derived interpretation, **not a reproduced bug or a
proposed parser fix**. Root must admit recording space and preserve an actual
RecordLimit/incomplete result, rather than tune a cap from this formula and
declare success or turn it into product policy.

The fixture reserves raw-token and observation space based on case count before
each wait and retains samples; its memory cost is not just the caller Vec or
`MAX_WAIT_RECORDS` scalar. Actual capacities, nested records and reporting work
need resource measurement. A lower record count from an incomplete run is not a
performance improvement.

## What the existing samples can mean

| Existing field | Source-defined boundary and limits |
| --- | --- |
| `wait_ns` | Immediately before `Poller::wait` to immediately after return. Includes that Poller/syscall/token work; excludes the fixture's preceding raw-record provisioning. It does not isolate kernel service or end-to-end round cost. |
| `capacity_before/after` | Retained caller token-Vec capacity. A change is not an allocation/reallocation/copy/fault count, and unchanged capacity does not prove the entire path allocation-free. |
| `write_completed_to_wait_return_ns` | Starts after each fixture write returns. Includes later source writes, record provisioning, prior waits/handlers and scheduling. Write completion is not a kernel readiness timestamp. |
| `wait_return_to_drained_ns` | Includes raw sample construction and prior token handlers plus exact read/drain verification. It is not per-source kernel notification latency. |
| FD acquisition/return | Sequential known process-count arithmetic with held writers/readers and Poller. It does not establish descriptor identity, kernel memory return, one-shot ownership or universal leak freedom. |
| nearest-rank n/p50/p90/p99/max | Summaries of the retained defined samples only. The wrapper suppresses distributions for an incomplete invocation. It provides no competing-baseline ratio or confidence/causal claim. |

Root must preserve `metadata.json`, raw build/baseline stdout/stderr,
`summary.json`, resolved Cargo.lock, exact source/lock/harness/binary hashes and
timeout/status/cleanup observations. The wrapper's literal-batch regex identifies
the current baseline; a future dynamic-storage candidate cannot be assumed
compatible with that source recognizer. No candidate/wrapper adaptation is
selected here.

## Owed root work and R01–R16

Complete-harness compile/review and all native runs are unrun here. Root still
owes oneshot/refusal ordering, registration/close races, hot-source/quiet/control
fairness, actual production caller recovery, allocation/copy/fault/CPU/parent/
kernel costs, meaningful guard removals/restoration and equivalent pinned
competitors before any architecture/size/performance claim. Existing resource-
lane instrumentation must retain its actual process/native/kernel scope; this
lane adds no duplicate allocator/counter framework.

The [original R01–R16 account](README.md) remains binding: no production or
harness edit, no reproduced-fix/number/algorithm decision, no runtime/cache/
adapter/crypto/host-root change, no explicit child spawn, no audit/profile/native
execution or self-main merge. Actual checks in this checkpoint are limited to
document/source identity, links, policy and named-path whitespace. Seven-target
and hardware/Windows coverage remain unverified. Source-only work continues;
local heavy builds/new full worktrees remain prohibited, no local target/cache/
build job is active, and root validation paths/paused Windows ARM are preserved.
