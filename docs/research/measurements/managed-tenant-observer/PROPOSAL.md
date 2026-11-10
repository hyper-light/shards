# Borrowed observer disconnect and exact retained-cursor reconnect

Implementation shape submitted to ROOT; no new helper mechanism approved here.
POLICY_READ IMPLEMENTER canonical main24f48ee5ebf198485117aa93bf47685ad7d7d53f,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
R01–R16 apply. ROOT owns all actual endpoint/VM/mutation/cleanup campaigns,
audits/profiles and main merges. No such run by this lane; no production decision.

Use the existing literal-native `managed-capture TOKEN step PORT WORK_MS` on
HARNESS member2, normally completing AGENT member1, files-only member0 and the
existing independently executable main workload. Reuse the existing checked
NETWORK/EXPOSE/CONNECT shape from the normal-exit fixture; port, finite host/helper
deadlines and new absolute record directory are explicit caller inputs. No new
guest program mode, process, syscall, permission, cache or lifecycle manager.

The contract already states that losing an observer does not lose lifetime
ownership. Actual `remote::managed::subscribe(scope, offset)` names the first
unconsumed record byte, not an arbitrary replay position. The endpoint retains
only the unacknowledged record and accepts that record's start or the actual ready
cursor. This is a finite resume contract, not durable arbitrary history:
`crates/remote/src/managed.rs` at f2da72b2, subscribe/service.subscribe/service.lost,
and `observer_loss_retains_only_the_unacknowledged_record_for_the_same_cursor`.

Proposed sequence:

1. Require member1 normal canonical End/EOF and retain both exact normal terminal
   requirements. Receive member2 Open and its initial deterministic Data. For
   each channel, ACK Open, retain the final initial Data record unacknowledged,
   save its actual wire start and raw canonical bytes plus the pre-record ABI
   StreamState/payload tail. The saved cursor is independently checked against
   observed offsets; do not invent or subtract a guessed frame length.
2. Close and join only member2's two borrowed observers through existing ordinary
   cancel. Retain the authoritative owner and independent main control. Execute
   exact main stdout/stderr progress after both observer joins.
3. Reconnect using the same authenticated owner/run/generation/member/channel and
   the saved start. Require the exact retained canonical record at that offset,
   with no additional Open, no skipped/repeated payload and correct canonical
   state. ACK that record; do not infer ACK processing from enqueue alone.
4. Only after replay is observed, release the existing owner gate. Require exact
   newly produced member2 fd1/fd2 bytes after release and another independent main
   exec. This avoids claiming production progress from delayed initial credit.
5. Capture raw wire/metadata/history as applicable, stop/remove the owned run,
   stop daemon, join observers and check exact FD return before strict metadata
   assertions. Member1 remains ended/tail complete; owner-stopped member2 remains
   aborted/tail unknown. No history, full population, reaping or kernel-return
   completeness is inferred from a cursor or channel EOF.

Required narrow test seam, **outside current host-parent edit ownership**:
Client currently hardcodes subscribe(scope,0). ROOT should approve/provide a
caller-supplied initial wire offset through Client/Observer::start/Fixture::observe
with existing callers default0; the new test supplies only an actually retained
start. No product protocol change. This seam must be approved before edits there.
`initial-offset-seam.patch` makes that exact test-only change reviewable against
87503's host parent. It is a PROPOSED patch artifact, not applied source: one
caller-supplied Client cursor, an extra Observer::start argument and
Fixture::observe_at, with the existing Fixture::observe wrapper default0. Actual
source search found only Fixture's caller of Observer::start, so an unused
Observer::start_at wrapper is omitted. ROOT owns approval/application there.

ROOT reported actual r606 failure before normal metadata: a pure Unit wake from
Control.finish sees the existing one-slot queue Full and triggers Output.abort /
Lost0 on final ACK. Refactor lane owns the authorized production fix. This
proposal's real body depends on that fix because step1 preserves fce's normal
End/ACK/EOF witness; ROOT was notified BEFORE reconnect component edits. Pure
retained-record composition can proceed independently. No weakened assertion or
duplicate production fix is authorized here.

Local observer join is not an independently observed remote close-processing
barrier. The real fixture must retain any actual typed refusal/deadline and raw
records; no guessed sleep, periodic probe, hidden retry or assertion waiver is
proposed to mask a race. Only actual ROOT results can establish that behavior.

ROOT actual negative/mutation gates: wrong retained cursor rejected with actual
typed reason, replay mismatch detected before release, disconnect never stops
owner/main, meaningful identity/cursor-check removals detect corresponding actual
wrong data/scope rather than only losing an alert. Refusal/timeout preserves raw
state and always performs owned cleanup. These are owed runs, not passing claims.
Light checks remain exact-source compile/pure only in a NEW private admitted
1CPU/1GiB/2GiB source/target. No local compile while Mac is below30GiB.

Primary sources: [current scoped endpoint source](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/remote/src/managed.rs),
[managed-runtime contract](../../../design/managed-runtime-plan.md).
External transport boundary: [RFC9000 section3](https://www.rfc-editor.org/rfc/rfc9000.html#section-3)
defines stream states; stream closure alone does not establish application lifetime.
That last application-lifetime distinction comes from Shards' contract, not QUIC.
