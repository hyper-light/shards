# Reviewed startup allocator phase proposal

Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Policy files remain at `f5cec365`, rules
SHA-256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Proposal base is own `96b6bb43bf0610497f426cfcbc4ae1654172090c`, with fixture
SHA-256 `31df9e2bc83270fc4fee35caaa34b885b5e12dcae551bbee9238cbdbf6e232b7`.
Root subsequently authorized this EXACT proposal and preserved
[patch](startup-hooks.proposed.patch). It is applied at
`4bc28256ee1285715e5573e9f4ba6b02caf436aa`, fixture SHA-256
`874e8b38b46479d4199ac53940a84a8ac6c8f7199998a4edb591eacab81c515e`.
[Implementation/light evidence](STARTUP-IMPLEMENTATION.md) distinguishes the
three shape-unit passes from all unrun root caller/real-VM/profile gates.
The remaining sections retain the reviewed design and historical source checks.

Root's [allocation033c record](../managed-coordinator-allocation/README.md) on
main `7c26e5292161fc891a7c6c6525305464a3a044be` reports aggregate setup allocations
before `before_fixture`; its N0/N1/N2 runs are functional pilots, one each.
Neither the aggregate nor the inspected calls establish an artifact-hashing
cause or authorize a production optimization. The proposal gives root narrower
observation intervals while preserving the existing workload.

## Exact proposed scope

Add two private, fixed-field structs in the existing Linux-only cost fixture.
Store snapshots on the stack using the existing test-only `MANAGED_ALLOC` and
existing `allocation_snapshot_record`; do not reset or replace the allocator.

| Snapshot | Placement |
| --- | --- |
| `test_entered` | First statement of the Rust test body, before `Inputs::supplied`. |
| `supplied_inputs_ready` | Immediately after the original supplied-input call. |
| Five `before_resolution` fields | Immediately before each original fixture/CLI/testguest/kernel/init resolver expression, in that order. |
| Five `after_resolution` fields | At entry to the existing artifact closure, after the path operand resolves. |
| Five `artifact_value_ready` fields | After the unchanged read/hash/JSON expression produces its artifact value, before returning it to the enclosing input JSON. |
| `inputs_recorded` | After the original `inputs.write("inputs.json", ...)` returns. |

The fixture's owned `current_exe()` result stays borrowed outside the new
resolution block; the four existing static-path getters remain in place. There
are no new retained `PathBuf` locals, repeated getter calls, warmups, helper
rebuilds, caches, tracing interfaces or OS adapters. The same artifact read/hash
expression and source/artifact identity values remain. The existing input JSON
macro still performs its own outer conversion/insertion between these named
boundaries; those costs are not silently assigned to the hash call.

Serialize `allocation-startup.json` only after existing teardown, allocation and
native-fault records, before the existing FD/exit assertions and unwind rethrow.
No snapshots move the existing protocol, shutdown, pinned identity, exit-watch,
FD-return or terminal checks. Existing later snapshots retain their names and
scope. This adds only test instrumentation and its deferred record cost.

## Meaning and limits

All fields are independent concurrent atomic loads, not coherent events or
quiescence. They cover every thread routed through the observer's Rust wrapper.
They do not cover daemon/VM/guest, foreign C, direct `System`, usable sizes,
copies, resident pages, native fault categories or kernel/process-tree totals.
The first snapshot still includes earlier process/libtest/setup allocations and
does not recover their call sites. Intervals locate observed aggregate activity;
they do not prove exclusive attribution to a resolver, read, hash or serializer.

Keep sticky `Unknown` accounting, raw cumulative fields, absolute live requested
bytes and recorded process-lifetime peaks. A difference is usable only when both
cumulative fields are known and ordered; peaks cannot be subtracted into phase
peaks. Extra stack state, snapshot loads and deferred serialization can perturb
the instrumented program. No zero-cost or allocation-free-instrumentation claim
is made for the whole caller.

An early input/host/artifact panic remains outside the existing protocol unwind
boundary and can leave no startup record. `NotReached` is used only for absent
fields in a record that is actually emitted. Missing output is not successful
cleanup or a zero measurement. No Drop logger, extra panic handling, process
termination or preflight artifact execution is added to recover such a record.

## Sources and alternatives

These are design inferences from source, not live behavior. Automated grades and
scores are the exact quote-check metadata from research
`01a1c5cfb146737ea308bd247e39f267`; they are not policy or validation scores.
The [full ledger](PLAN-RESEARCH.md) retains all checked quotations.

| Basis | Grade / score | Implication for this draft |
| --- | --- | --- |
| [Rust operand evaluation order](https://doc.rust-lang.org/reference/expressions.html#evaluation-order-of-operands) | C / 68, inferred | Put the before snapshot in the existing path operand, before the snapshot-field borrow. |
| [Rust temporary scopes](https://doc.rust-lang.org/reference/destructors.html#temporary-scopes) | C / 68, inferred | Respect Rust2024 block-tail temporary scope; move the owned result out before borrowing it. No claim of identical generated code. |
| [serde_json v1.0.151 macro](https://raw.githubusercontent.com/serde-rs/json/v1.0.151/src/macros.rs) | B / 72, inferred | Preserve sequential artifact evaluation and distinguish the returned value from outer JSON conversion/insertion. Cargo.lock pins this version and its registry checksum. |
| [Root033c receipt](../managed-coordinator-allocation/README.md) | B / 70 | Cause and optimization remain open despite substantial setup totals. |

The official versioned1.98.1 Reference URLs were inaccessible through web open;
the language links above are current official pages retrieved2026-11-10. This
does not establish a changed-caller compiler result. Cargo's `rust-version` is
MSRV, not evidence of which formatter/compiler a closing job used.

Keeping only the existing `before_fixture` would leave setup undivided. A single
additional pre-artifact snapshot would still combine resolution and artifact
record creation. A richer profiler such as the previously researched `dhat` can
give root call-site evidence later, but would be a different root execution and
is not introduced here. Fixed fields serve the existing five real calls without
adding a generic observer framework or choosing an optimization.

## Source-only checks and root gates

[Source-check record](records/startup-proposal-source-checks.json): rustfmt on
stdin/stdout only, `git apply --check` only, and text comparisons of the proposed
source. Actual formatter was `rustfmt1.9.0-stable (48a229ceae2026-09-01)`; local
Python was3.9.6. The original `run` body and cleanup/exit/FD/terminal tail are
byte-identical except the added post-teardown record. Each resolver remains
once and the artifact read/hash expression is identical. These checks are not
typechecking, a full caller test or an execution of an artifact getter.

If selected, root still owes actual full source-matching caller/helper builds,
normal tests, supported-target checks, startup/native failure paths and exact
identity/exit/FD/protocol/shutdown cleanup validation on the changed source.
Root executes and interprets all isolation/profiling, paired/interleaved
same-N release runs with caller-supplied n, perturbation/noise analysis,
mutations/restoration and physical parent/kernel attribution. The existing paired
runner does not yet ingest this proposed new file. Its optional extension needs
the accepted record contract first; no runner change or execution is bundled.

R01/R04/R06/R07/R13: test-only fixed reached/not-reached fields and unchanged
control body; actual caller correctness remains root-owed. R02/R03/R05/R12:
primary sources, existing caller/wrapper, no production literal/cache/adapter.
R08/R14: no performance, cause or completeness claim. R09/R10: instrumentation
proposal rather than an inferred product fix; source checks and unrun work are
retained. R11: no new portability/native result. R15/R16: no children, audit,
profile, VM, main merge, local build, target/cache creation or new worktree;
root jobs, all evidence and paused Windows ARM remain protected. The sustained
lane and parent goal stay active for root's next approved scope.
