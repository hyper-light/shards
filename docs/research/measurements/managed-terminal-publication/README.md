# Managed terminal publication: scoped implementation checkpoint

ROOT reproduced the final metadata defect in actual Fedora r668 at
`eaf0982b4716f320b34162d3a366f0116caf0607`. Both member1 channels delivered
canonical End, accepted the final application ACK and produced transport End.
The guest gate release, causally later sibling fd1/fd2, independent main exec,
stop/remove, owner/observer join and observer FD7→7 all succeeded. The strict
test still exited101 at tenant_exit.rs:340: expected ended/complete, observed
aborted/unknown. Member-ended and pre-stop metadata were header-only; stopped
member1 rows were102/102 and99/99, both aborted/unknown. Continuing siblings
were177/177 and171/171 aborted/unknown. `protocol_completed:true` was not a pass.

The exact source/command/artifact/raw records are on base main4bfaceb38 in
[normaleaf0](../managed-coordinator-boundaries/normaleaf0/), retained by ROOT.
ROOT reported a25,310-byte archive SHA256
`3b4958ce2b5b475600a681fed7b0694da5b2103a00117afc2076d4ebac9e6bd0` and verified
every transferred member. This lane read those actual records and the existing
Life/credit/Shared paths before implementing. Its pure checks below supplement
that actual red; they do not rerun or replace it. Gap labels remain G01–G24 and
P0–P13; this is scoped P3 terminal/capture ownership work, not plan closure.

## Identity and authority

POLICY_READ IMPLEMENTER `01a1c5d2ee5871a2a9a9cdc9f56793f0`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Full canonical AGENTS.md,
development-rules.md R01–R16, development-evidence.md, CLAUDE.md,
managed-runtime-plan.md and `/Users/adalundhe/g121-handoff.md` reread after
compaction. Canonical base `4bfaceb3887e857b2d1e03abd533bccc2833e681`, later
docs-only canonical `1d0588b28f839fd0fec049e4bcb00e261e28c3aa`; policyfiles
f5cec365, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Readback was sent before source edits (ROOT queue seq5911), then renewed in
commentary and queue seq5926 when ROOT requested indexing confirmation.

Owned source is only `crates/shards/src/daemon/managed.rs` Life/credit and
`crates/remote/src/managed.rs` Shared-derived state/finish guard; evidence is
this directory. Existing worktree reused on `p/managed-terminal-publication`.
e1cb/611777 proposals and82425 ABI/operator/Poller branches remain preserved.
No main merge, child/automatic goal dispatch, local build, full worktree, ROOT
clone/target, paused Windows ARM VM, privilege, trust or crypto change.

## Decision and established alternatives

The complete earlier proposal is retained at
`e1cb83139551b404736bc036277ccdb536ad409d:docs/research/managed-terminal-proposal.md`;
its ordering supplement is at
`61177791a88355fc58634bbd9b30003f0aab14ca:docs/research/measurements/managed-terminal-linearization/PROPOSAL.md`.
Both were read in full. Their source predictions were not implemented until
ROOT supplied the distinct actual metadata red and explicit authorization.

Keep three facts separate: canonical End/Aborted(errno), remote wire state and
cursor, and the logical Life outcome. [RFC9000 §§3.1–3.2/4.5](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.1)
separates stream finish/final size from application consumption. Pinned
[hyper-raft233b7fce send state](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/send.rs)
keeps pending FIN and acknowledgement distinct. The existing Shards application
Consumed cursor is an authenticated peer claim about contiguous encoded record
bytes, not payload offset, durable storage, process wait or population reap.

Alternatives considered before choosing this existing seam:

| Alternative | Reason for this choice |
| --- | --- |
| Continue publishing only when endpoint.run returns | Actual r668 leaves a completed normal channel unpublished, then stop overwrites it as aborted. |
| Stop the driver immediately on a final ACK | Couples logical metadata to transport retirement and can discard FIN retransmission state; no new FIN-ACK barrier was established. |
| Publish from Shared's inline callback | Would add Life/metadata work to notification and risk held-lock reentry; callback now only coordinates the existing credit wake. |
| Periodic probes, cached terminal flags or another manager/thread | Existing Shared state and credit Condvar already provide the predicate/notification; no additional owner or periodic work is needed. |
| Derive success from equal counters | Ready, Finished and Aborted can all have equal cursors. The new typed observation explicitly separates them. |

[Rust Condvar::wait](https://doc.rust-lang.org/std/sync/struct.Condvar.html#method.wait)
provides atomic unlock/wait and allows spurious wakeups; notification itself is
not completion. [Rust Mutex::lock](https://doc.rust-lang.org/std/sync/struct.Mutex.html#method.lock)
does not permit same-thread lock reentry. These contracts informed the ordering
below, rather than serving as empirical liveness proof. Existing wake coalescing
and its primary alternatives remain documented in the landed
[finish-notify record](../managed-finish-notify/README.md). Generic Sender,
Producer::submit, queue capacity and protocol constants remain unchanged.

## State and ownership

`Control::state()` derives an ephemeral State plus Progress under one existing
Shared lock: Pending, Ready, Finished or Aborted. Nothing stores that observation
as a second ledger/cache. Ready replaces the daemon's duplicated Available/Send
bookkeeping; it has the same predicate as the existing nonzero producer capacity
check (Ready slot with Open ending and an already admitted nonzero allowance).

Canonical Terminal::{End,Aborted(errno)} survives After and Life::Draining.
Life::Aborted retains an optional known canonical terminal; None means none was
successfully handed to finishing before interruption. No errno is manufactured
for an owner stop or transport loss.

| Life and observation | Resolution |
| --- | --- |
| Serving, validated terminal successfully submitted | Draining(canonical cause), before invoking finish |
| Draining(End), typed Finished | Ended, one ended/complete row |
| Draining(Aborted(errno)), typed Finished | Aborted with that errno, tail unknown |
| Draining, Pending/Ready | Remains draining on completion observation; stop resolves aborted/unknown with known cause |
| Serving or Draining, typed Aborted | Aborted/unknown; existing worker/endpoint-return path revokes credit and requests owned stop |
| Ended or Aborted, any duplicate completion/stop/endpoint return | Logical outcome remains sticky; stop still requests transport retirement |

Stop and completion both resolve through Life. While holding Life, stop reads
Shared before it may overwrite that remote state. If Finished is already known
for canonical End, it resolves Ended even when the credit worker has not run.
If the snapshot is still pending, that observation is the stop cut; a later ACK
cannot revive the now-aborted Life. Cursor metadata uses that same observed
snapshot, not a later unrelated read. No post-abort finish can resurrect Shared:
Shared::finish rejects either an Aborted slot or Aborted ending under its mutex.

Logical completion does not imply that the owned endpoint has stopped. Every
Output.abort, including Drop and endpoint return/error, requests transport stop
unconditionally; terminal idempotence only suppresses a second logical row.
Endpoint return/error while Serving or while draining canonical Aborted still
converges through this path. Typed remote Aborted in Waiting/Captured/Draining
also makes a completion action actionable, so credit cannot be stranded waiting
for capacity. A fully delivered canonical Aborted still keeps its known cause
and uncertainty; it is not relabeled a normal End.

## Lock graph, wake and metadata

The allowed held-lock edges are credit→Shared for the current predicate and
Life→Shared for outcome arbitration. Shared publication releases Shared before
the inline callback takes credit. There is no held Shared→credit or credit→Life
edge. Callback registration/waker lock is released before invoking its callback.

The callback upgrades only weak credit ownership and calls Credits::wake. That
method takes the same credit mutex used by the predicate and Condvar, not Life,
learning or metadata. It does no I/O, joins or ACK waits. If Shared changes
before predicate observation, the worker sees it. If it changes afterward, the
callback cannot pass the credit mutex before the worker atomically unlocks and
waits. Every wake rechecks the predicate; zero capacity does not suppress terminal
notifications. Pending drains sleep on the existing Condvar, without polling.

Credits.next drops credit and its temporary Shared observation before returning
an action. The existing worker then touches Life/learning/metadata. Finish marks
Draining and releases Life before its synchronous Shared notification. Stop
releases Life before credit revocation/notification or metadata I/O. The existing
learning observation moves to worker grant/submit/completion actions; it still
requires the sample's consumed wire cursor and retains its start/byte count. Its
observed interval now includes worker dispatch delay. No wire-ACK timestamp or
equivalent performance estimate is claimed; ROOT owns cost/learning validation.

Normal terminal rows keep the exact old fields. Canonical failure adds a separate
`member M channel C canonical aborted errno N` row, while its terminal row remains
aborted/unknown. Existing strict normal fixture assertions are unchanged.

Metadata writing returns errors. Publication failure requests channel stop and
logs member/channel plus the I/O error after releasing metadata/Life/credit
guards. An abort requests stop before attempting its append. A partial/failed
append is not retried or called published; logical consumption remains sticky.
[Rust Write](https://doc.rust-lang.org/std/io/trait.Write.html#method.write_fmt)
can return an I/O error, including after partial output. No sync/durability or
unbounded-filesystem-stall guarantee is introduced. Full fault/cleanup behavior
and all metadata readers remain coordinator gates.

## Actual light results and exact limits

NEW private Fedora source/target only:
`/home/adalundhe/managed-terminal-publication-light-4bfa-v1/{source,target}`.
Kiln r21/r22/r23/r24 each requested1CPU/1GiB/2GiB headroom; no overlapping lane
build, build/test jobs1, incremental/debug information disabled. Admission is not
a hard quota or a CPU/RSS peak measurement. Final retained directory397,348KiB.
Linux6.19.10-300.fc44 amd64/glibc, rustc1.99.1 commit58cf8f9c3/LLVM23.1.1 and
cargo1.99.1. Exact raw environment/toolchain and commands are retained under
[raw](raw/); [MANIFEST.json](MANIFEST.json) verifies all48 transferred members.

| Run | Actual result |
| --- | --- |
| r21 candidate edge before its correction | Exact Life/credit fragment fixture compiled0, endpoint-loss test101: Serving+typedAborted returned None instead of Aborted(None). This is a candidate regression witness, not an additional product VM reproduction. |
| r22 corrected candidate | Ten exact Life/credit/metadata fragment tests passed0. Real remote crate's three `managed::tests::terminal_` pure tests passed0; scoped real remote lib/tests Clippy0, policy0 and diff-check0. |
| r23 additional Output calls | Fourteen fragment/call tests passed0. Fixture Clippy failed101 on an unused OpenOptions import in its non-test build; this was fixed in the fixture only. |
| r24 final bytes | Fourteen fragment/call tests passed0; fixture Clippy with actual workspace panic lints0, explicit110-column Rust2024 rustfmt check0, policy0, diff-check0. |

The standalone [pure.py](pure.py) extracts exact production Life, credit,
metadata-writer and selected Output methods, plus actual ABI channel/scope and
remote State definitions. Source and every fragment hash are retained. Its
[Output shell](output-calls.rs) deliberately supplies snapshots and records stop
requests; it implements no driver or Shared transitions. Four call tests cover
unconditional repeated stop requests, endpoint-return paths, a real read-only
fixture-file write error with logged scope and continued stop intent, and Life/
credit release before an inline finish callback. These prove method call/state
behavior, not real transport retirement, descriptor return or complete-daemon
compilation. The other ten tests cover the r668 ordering, early stop, equal-cursor
uncertainty, errno, duplicates, zero-capacity drain, predicate/wait seam, wake
without readiness, and revoke/close convergence.

The actual remote crate tests use real Service/Shared code without constructing
or running native Endpoint/driver/socket helpers. They separate same-cursor
states, observe Finished after Shared release in the real callback, reject
duplicate terminal ACK and prevent finish after abort. The filtered sparse copy
retains actual remote/ABI/IPC/testdir crates, pinned provider and dependencies;
only private workspace membership is narrowed. Ordinary AWS-LC build notices and
the unused-rquickjs patch warning remain in logs. No provider option was changed.

The retained driver-r21.py through driver-r24.py are exact executed Python
payloads copied after execution; the outer command was `python3 - <<'PY' ... PY`.
Embedded bundles retain the submitted bytes. The first local rustfmt invocation
omitted edition2024 and reported preexisting let-chain parse errors; the explicit
edition/config invocation corrected this without a build. No layout noise from
default100-column formatting remains. Source hashes and all actual red/green/
fixture-failure logs are preserved; no failed run is reported as passing.

Final source SHA256:

| Path | SHA256 |
| --- | --- |
| daemon/managed.rs | `2b3e4d2c80be615a58b062b6a077c11511e2679c738c798a27f0e014a880f655` |
| remote/managed.rs | `79c949b781e71821079a0858c67270355ed24a231789aafad80c9695f6226fdd` |

No ABI source changed. The separately unmerged82425 tests still change ABIIDENTITY
through existing build.rs; adoption requires all host/guest helpers rebuilt
together. This checkpoint does not silently compose that branch or probe logging.

## R01–R16 account and ROOT-owed checks

| Rule | Scoped evidence and remaining gap |
| --- | --- |
| R01 | No new production unwrap/expect/assert, unchecked indexing or numeric limit. Poison-tolerant existing mutex/Condvar path retained; I/O errors handled. Scoped real-remote and exact-fragment panic lints pass. Full daemon/combined compilation, cold OOM and universal panic freedom are not proved. |
| R02 | Actual ROOT red precedes authorized fix; full proposals, RFC, pinned transport and official Rust contracts/alternatives above. Coordinator review remains required. |
| R03 | No new production numeric constant or capacity increase. Structural admission continues using actual size_of; exact host/kernel costs unmeasured. Test inputs are fixture values, not policy numbers. |
| R04 | Canonical typed terminal/errno, derived remote State, Life transition owner and explicit Draining credit/action; early stop and duplicate/failure tests above. Wire format/strict normal assertion unchanged. |
| R05 | Existing credit Condvar and one Life owner; duplicate Available/Send readiness removed, no new polling/cache/worker/manager. Wait graph and learning-observation timing limit above; real liveness/cost remain owed. |
| R06 | Focused Life resolution, completion action and error/stop methods; small exhaustive state cases, no unrelated framework. Full caller/control review belongs to ROOT. |
| R07 | Comments explain arbitration, post-unlock wake and stop-before-I/O obligations; no new unsafe code. |
| R08 | No comparative measurements or4–5× claim. All equivalent baseline/distribution work remains ROOT-owned. |
| R09 | ROOT r668 actual source-matching strict metadata101 is the issue reproduction; independent candidate-edge red retained separately. No speculative Producer fix. |
| R10 | Actual source/commands/hashes/raw red, pure greens, failed fixture lint and final corrections retained. Original fixture fixed-green and meaningful guard-removal/restoration remain ROOT-owned/unrun here. |
| R11 | Only GNU amd64 light results here; seven-target and real KVM/HVF/WHP combinations unverified for this source. macOS arm64 only; paused Windows ARM untouched. |
| R12 | Shared Rust seams only, no OS adapter, extra installed binary, authority/crypto change or duplicated OS mechanism. Fixture binaries are evidence tooling only. |
| R13 | Existing owner, credit worker and notifications; no parallel lifecycle or stored completion/progress copy. Optional canonical cause has explicit meaning; errno is not a stop sentinel. |
| R14 | No CPU/allocation/copy/fault/latency profiling or performance inference. ROOT owns all measurements, including callback-to-worker observation cost. |
| R15 | Implementer scoped source/light only; no children, auto-goal expansion or main merge. Signed named-path checkpoint/push to ROOT; complete managed plan and lane remain active. |
| R16 | No active local target/cache, local compilation, new full worktree or deletion. Preserved every prior checkpoint/frozen source and protected ROOT paths; remote private source/target retained. Local floor holds were obeyed. |

ROOT still owes: full actual daemon/native caller and strict normal-tenant run
with rebuilt source-matching helpers; pre-stop/stopped exact rows, sibling/main
progress and real endpoint/join/FD/handle/kernel return; independent canonical,
ACK, sticky-stop and unconditional-retirement guard removals plus exact restores;
withheld ACK, endpoint loss while Serving/draining canonical Aborted, ordinary
disconnect/reconnect, duplicate events, revoke-before/after-completion and owner
stop before/during/after finish; real metadata-failure/slow sink behavior and
reader compatibility; supported seven-target and hardware gates. No closure or
durability/performance/resource-return claim is made from these light tests.
