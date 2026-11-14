# Control adversarial review

POLICY_READ — role: **adversarial reviewer, area `control`**, reporting to ROOT (main Shards coordinator); no implementation role assumed.

- Head: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`; base: `79caf2d743d4c3c917f207bd650655a523847bf4`.
- Worktree: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-control` (detached; clean at the time of review).
- `AGENTS.md` SHA-256: `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`.
- `docs/development-rules.md` SHA-256: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
- `docs/development-evidence.md` SHA-256: `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`.
- `CLAUDE.md` SHA-256: `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.
- Read `/tmp/g121-shot/review/brief.md` first and in full, then the four policy/mechanics files. `origin/main` and HEAD identify the same revision. All delegated source reviewers returned this policy revision/hash and read-only scope acknowledgments.
- Compared the area's changes with `git diff 79caf2d74...HEAD`, and inspected the paths at head. Review included standards and specification axes, D238/D240/D244/D251/D252/D254, M559/M563 and the managed-output/API lane tests.
- **Validation boundary:** every finding below is verified by tracing source/state transitions, with a concrete test witness proposed. None was executed or reproduced live. The binding brief forbids builds, tests, daemons, VMs and heavy work; ROOT owns that validation. The review changed no code and made no commits, pushes or PRs. It originally wrote only `/tmp/g121-shot/review/control.md`; on 2026-11-13 the user explicitly requested this durable copy under `docs/bugs/2026-11-13/`. `scripts/check-development-policy` was not run because this is a source-only review, not an implementation readiness claim.

Priority mapping for this report: P0 critical, P1 high, P3 medium, P5 low. P2 and P4 are unused. Counts: **P0 1; P1 2; P2 0; P3 4; P4 0; P5 1**.

## Findings, most severe first

### control-01 — P0 / critical — accepted output is truncated when the run ends

**Location:** `crates/api/src/image/session.rs:1054–1055`; related `:230–235`, `:1042–1045`, `:1145–1146`, `:1220–1234`.

**Defect:** The member output pump stops on a failed credit after writing one queued record, even when more records and the channel's end have already been accepted.

**Concrete scenario:** The answer reader accepts `OUTPUT(A)`, `OUTPUT(B)`, `OUTPUT_END`, then `ENDED` for a member channel before that channel's pump resumes. `ENDED` removes `shared.to_host`. The pump pops and writes A, calls `shared.credit`, gets BrokenPipe, and returns. Its pipe writer closes; the owner reads A then EOF, while B is still in `flow.queued`. No malformed guest or lost guest tail is required. Multiple small records fit within the initial byte credit, so one-record guest credit does not exclude this schedule.

**Evidence:** `pump` first removes queued data at :1042, writes it at :1050, then unconditionally returns if `shared.credit(...).is_err()` at :1054. `Shared::send` returns BrokenPipe when `to_host` is None. `read_answers` removes that handle on `ENDED` at :1146, while `take(OUTPUT)` and `take(OUTPUT_END)` queue the complete channel normally. D240 promises exact captured bytes. Its allowance for aborting an owned guest channel at run end does not permit losing bytes already accepted before `OUTPUT_END`.

There is also an ordering hazard in this same completion path: `api_run.rs:3844–3855` sends the run's `ENDED` without joining the capture reader; `pump` at :1045 treats the run's end as sufficient for channel EOF, although `read_answers` continues accepting output afterward. The two-already-queued-record witness above establishes the defect without relying on that additional race.

**Confidence:** High, source-proven; live reproduction unrun.

**Fix direction:** Drain accepted channel output independently of whether further credits can be sent. Coordinate terminal output state with the answer reader/capture completion rather than treating `ENDED` as proof that no channel bytes remain.

**Catching test:** Gate the pump, have a controlled host emit two distinct output records, `OUTPUT_END`, and `ENDED`, wait until the answer reader has accepted them and closed `to_host`, then release the pump. Require the read-to-end bytes to equal the concatenation of A and B, exactly once. Also gate the capture thread so `ENDED` precedes its last host-side output and verify no premature EOF. Existing empty-output-at-run-end coverage (`a_run_ended_before_its_first_credit_still_has_its_outputs`) cannot catch this.

**Status (2026-11-13):** fixed in cf2764c6e (red `image::session::tests::output_taken_before_the_runs_end_reaches_its_owner_whole` and `output_said_after_the_runs_end_reaches_its_owner_before_its_end`, d7b62bd7d; adas-mac-studio-3.local macOS arm64: the owner read A alone, and nothing, before; A then B, and the record said after the run's end, after; both guards' mutations red; shards-api clippy clean). Real-VM managed_output and api_image lanes remain ROOT's; see [evidence](../../research/measurements/control-01-v1/README.md).

### control-02 — P1 / high — aborting a pending socket lane leaves its workers and frees its admission

**Location:** `crates/remote/src/control/unix.rs:573–577`; related `:186–203`, `:533–539`, `crates/shards/src/api_run.rs:3943–3962`, `crates/shards/src/warm.rs:2379`, `crates/shards/src/workload.rs:1585–1592`, `crates/init/src/lanes.rs:69–74,111–114`.

**Defect:** Resetting an already handed-off socket ask releases its endpoint slot but never cancels the host/VM/guest operations opening that socket.

**Concrete scenario:** The workload listens on a granted Unix socket but does not accept; fill its backlog. An authenticated controller opens a lane to that name, then resets it while init's blocking `UnixStream::connect` waits. `stop_socket` removes `Stage::Asked`; only `Stage::Carrying` has an abort action. The host remains in `rx.recv`, the VM waits for the token's connection, and the detached guest worker remains in connect. Repeat connect/reset: each reset makes room under `admission.lanes`, while prior workers, token entries, descriptors and host lane entries remain. Resource use is therefore not bounded by the admitted lane count.

**Evidence:** `Connect` contains a name, lane id, reply sender and wake, with no cancellation channel. `admit_socket` counts the endpoint's remaining lanes, while :573 removes the pending lane immediately. Init connects before it dials the host, so the host cannot cancel that wait by closing an already established relay. D251 promises an abort ends the socket and that endpoint teardown ends its sockets; R04 requires cancel paths and R15 requires resource-return witnesses.

**Confidence:** High, source-proven; live thread/fd growth unrun. Run termination eventually releases host waits; this finding concerns per-lane abort while the run remains alive.

**Fix direction:** Propagate request cancellation through endpoint, host, VM and guest into a cancelable connect; hold admission until the worker acknowledges termination.

**Catching test:** With one admitted lane, fill the granted socket's backlog, open then reset a pending lane, and repeat beyond one lane. Observe guest/VM/host thread, fd and token-registry counts returning after each abort, without accepting connections or ending the run.

**Status (2026-11-13):** fixed in 3c28b68a78f7deaa9ac3db5046bb4df1ea4c53f1 (red `lanes::tests::a_full_socket_backlog_refuses_a_lane_instead_of_holding_its_worker`, bcc79fccdcce043e6f504063463598acfe8664fb; adacachy2 Linux x86_64: retained worker/Timeout before, EAGAIN after; all 6 lane tests and shards-init clippy passed). Full relay-abort integration remains ROOT's; see [evidence](../../research/measurements/control-fixes-v1/README.md).

### control-03 — P1 / high — the MCP stdio bridge cannot return after its first copy failure

**Location:** `crates/init/src/mcp.rs:201–205`; related `:170–172,188–197`.

**Defect:** A failure in either bridge direction does not interrupt the other direction's blocking I/O, retaining the bridge process, helper and descriptors.

**Concrete scenario:** Keep the harness's writer of bridge stdin open and idle. Close the harness's reader of bridge stdout, then let the controller send a response. The downward `io::copy` returns BrokenPipe. `copy_both` then joins the upward worker, which is blocked reading the still-open stdin pipe. It never returns status 1. The symmetric case is an upward input-read failure while the main thread waits for socket output.

**Evidence:** :197 calls blocking `io::copy(input, up_socket)`; :201 calls blocking `io::copy(down_socket, output)`; :202 immediately joins up before returning `down.and(up)`. No cancellation covers stdin or stdout. The bridge contract at :170–172 says either direction's failure returns status 1, and :188–190 says the first failure ends it. D254 requires bridge/lane cleanup, with each connection independently owned.

**Confidence:** High, source-proven; live reproduction unrun. This witness uses BrokenPipe, not an orderly half-close, so D254's legitimate independent half-close behavior does not refute it.

**Fix direction:** Make all three I/O endpoints cancellation-aware and cancel the opposite operation on the first failure. Shutting down only the socket cannot wake a read on the separate stdin pipe.

**Catching test:** Hold a pipe's input writer open, arrange a downward writer that fails on response bytes, and require `copy_both` to return Err and join its helper before the input writer is closed. Add the symmetric input-read-error case.

**Status (2026-11-13):** fixed in f7d83f73a (inithex-03's overlapping fix, preserved when merging main; red `mcp::tests::a_bridge_output_failure_does_not_wait_for_its_idle_input` at dee36cb5f8fd70e2e90551ec66b9aa1938dfd60b). This lane reproduced the same Timeout with live stdin, then found the landed descriptor-owned fix during the required main merge. The independent shared-relay candidate was superseded before landing. Retained upstream guards cover stdout failure, peer end and normal half-close; this lane adds symmetric input-error coverage. [Evidence](../../research/measurements/control-fixes-v1/README.md), [landed fix evidence](../../research/measurements/inithex-fixes/README.md).

### control-04 — P3 / medium — an idle stdin feeder survives member/run termination

**Location:** `crates/api/src/image/session.rs:1081`; related `:1772–1799`, `:1286–1303`, `:1942–1957`, `:1973–1983`.

**Defect:** Once the detached input feeder blocks reading the owner's pipe, member/run termination cannot close that reader or reclaim its thread and shared state.

**Concrete scenario:** Take a member's input, wait until input room permits the feeder's first read, and retain the returned writer without typing. End the member, stop the run or drop its session. State changes notify a condvar, but the feeder is blocked in `PipeReader::read`, not that condvar. It retains its reader, `Arc<Shared>`, buffer and any diagnostic file kept in Shared. A first subsequent write within pipe capacity can succeed even though the member has ended. Repeated ended sessions with retained input handles retain these resources.

**Evidence:** `take_input` spawns the detached `feed` with a Shared clone at :1792–1798. `feed` checks termination only before entering its blocking read. `INPUT_ENDED`, `ENDED`, `stop` and `Drop` have no read-cancellation handle. The explicit contract at :1060–1062 promises termination drops the read end so caller writes fail; R04/R15 require cleanup.

**Confidence:** High, source-proven; actual resource counts unrun. Closing/using the owner writer can eventually let the thread finish; that is not lifecycle cancellation.

**Fix direction:** Own a cancellation-capable feeder with the session/member lifecycle and wake the actual pipe read when that lifecycle ends.

**Catching test:** Synchronize at entry to the idle read, end the member and then stop/drop the session while keeping its writer open. Require the reader/helper to be gone, subsequent writes to fail, and retained descriptors/shared state to return.

### control-05 — P3 / medium — cap-loss metadata grows forever and log opens load it whole before charging

**Location:** `crates/shards/src/daemon/managed/cap.rs:253–265`; related `crates/shards/src/daemon/managed/log_lanes.rs:84–89,118,234`.

**Defect:** Every cap eviction appends permanent history to `log.capped`, and each log open reads that entire history into an uncharged String.

**Concrete scenario:** A long-running run repeatedly has member segments removed by its shared cap. Output segments remain bounded, but one metadata line per removed segment accumulates indefinitely. Each new log lane allocates and scans the full historical file in `opened -> capped`, before `Held::new` admits/charges the lane. A small output cap therefore does not bound this persistent history or a log-open allocation, even for a lane later refused for capacity.

**Evidence:** `record` opens `CAPPED` in append mode and writes segment/bytes. `capped` uses `read_to_string` and sums every line. Repository references to `CAPPED`/`log.capped` provide no compaction or cumulative-counter rewrite. D240 excludes indexes from output accounting, so this is an unbounded history/allocation finding, not an assertion that retained segment output exceeds its stated cap. R01/R14 require fallible bounded allocations and resource accounting.

**Confidence:** High, source-proven; disk/memory measurements unrun.

**Fix direction:** Keep durable bounded cumulative loss counters (with appropriate atomic update/recovery), and include any remaining read allocation in admission accounting.

**Catching test:** Produce many cap evictions while keeping tiny segment retention; verify metadata storage and log-open allocations do not grow with total lifetime eviction count, including an open refused for budget.

### control-06 — P3 / medium — a stalled attached viewer retains a daemon thread after the run ends

**Location:** `crates/shards/src/daemon/managed/drain.rs:595`; related `:472–492`, `crates/shards/src/daemon/managed.rs:2036–2038`, `crates/shards/src/daemon.rs:4220–4223`.

**Defect:** The per-run attached-output viewer writes through a blocking File with no cancellation tied to run termination.

**Concrete scenario:** An attached client's stderr reader stays open but stops consuming. Produce enough drained member lines to fill the pipe and block `line` in `write_all`. Stop/remove the run. `Drain::close` sets state and wakes its condvar, but cannot interrupt the write. The `managed-shown` worker retains the output File, its viewed logs and Arc<Drain> until that client reads or closes its pipe; repeat with further attached runs to accumulate workers/fds.

**Evidence:** The daemon clones attached stderr into a File at :4223 and spawns `drain.show(&mut to)` on its daemon scope. `line` calls `to.write_all(whole)` at :595. End/abort only changes Drain state; there is no cancellation descriptor in that write. D240 requires a slow viewer not to hold capture up; capture independence holds, but R04/R15 cleanup does not. The stalled-viewer test releases its synthetic blocked writer before joining, so it does not establish end-time cleanup.

**Confidence:** High, source-proven; actual post-removal fd/thread observations unrun. This does **not** claim daemon shutdown deadlocks: the daemon uses process::exit, bypassing scoped-thread joins.

**Fix direction:** Use a cancellable output write tied to viewer/run lifetime without backpressuring capture/log retention.

**Catching test:** Keep a real attached stderr pipe reader open and unread until its viewer blocks. Stop/remove the run without releasing the pipe; require the viewer and its run-owned descriptors to return.

### control-07 — P3 / medium — exec lanes reject valid specs above the unrelated 4 MiB frame bound

**Location:** `crates/remote/src/control/exec.rs:125`; related `:53,121`, `crates/remote/src/frame.rs:13,137–142,153–161`, `crates/abi/src/run.rs:48–60`.

**Defect:** Exec ask framing is capped at 4 MiB even though the lane contract and its own size check admit a 6 MiB run spec plus scope.

**Concrete scenario:** Build a Spec with `/bin/true` and 70 arguments of 65,536 bytes: its encoding is above 4 MiB and below 6 MiB; every string is below Linux's per-argument bound. `Ask::frame` passes `ASK_MOST` then fails in `frame::put`; `Execs::exec` maps this to a protocol refusal. Manually encoding it fails endpoint `frame::peek` too. A guest with adequate stack soft limit can accept such argv under Linux's 6 MiB aggregate bound.

**Evidence:** `ASK_MOST = SCOPE + run::MAX_PAYLOAD`; run MAX_PAYLOAD is 6 MiB. The shared frame encoder/parser both impose `MAX_FRAME = 4 << 20`. D238 at architecture.md:26149–26152 and :26648 explicitly promises the run-protocol bound. This is a bound mismatch, not a claim that the default stack limit always permits every 6 MiB command.

**Confidence:** High, source-proven; live large-argv exec unrun.

**Fix direction:** Derive and apply the exec-ask encoder/parser limit consistently from the run spec bound plus its 44-byte scope.

**Catching test:** Construct the spec above, assert `4 MiB < encoded length < MAX_PAYLOAD`, and require `Ask::frame`/`Ask::decode` round-trip plus endpoint admission. Add a real guest case with the appropriate stack limit.

### control-08 — P5 / low — log-follow fallback adopts an explicitly arbitrary polling interval

**Location:** `crates/shards/src/daemon/managed/log_lanes.rs:287`; value `crates/shards/src/daemon.rs:252`.

**Defect:** A log lane without a file watch polls every fixed 250 ms, a value expressly documented as picked and awaiting replacement rather than learned or protocol-derived.

**Concrete scenario:** Exhaust the user's Linux inotify instances, then follow a log lane. `FileWatch::new(...).ok()` falls back to `wait(...Some(UNWATCHED))`, giving idle wakeups and append detection latency determined by the picked 250 ms interval, regardless of host load or cold-start behavior.

**Evidence:** daemon.rs:246–252 calls this a 'figure picked, owed its replacement'; architecture.md:8319 likewise says 'Still picked, owed'. The new log-lane path adopts it at :287. This directly violates R03 and the brief's explicit instruction to flag fixed production polling values. Reusing an older value does not provide its missing derivation.

**Confidence:** High for the documented policy violation; no measured latency/CPU claim is made.

**Fix direction:** Prefer a resource-return/watch-availability notification and make any required fallback behavior learned/data-derived, including cold start; retain its derivation/evidence.

**Catching test/evidence:** Force watch creation failure and observe the selected wait and cancellation behavior; verify the replacement follows its measured/derived method rather than a hardcoded duration. ROOT must measure idle/load/cold-start behavior.

## Checked and found sound

- In the inspected endpoint paths, the enrolled controller key is pinned before lane processing; run/generation checks and host-active-member checks prevent a lane selecting another run/member by merely changing its identifiers. No cross-owner opening was found. Member MCP identity originates in init's member-specific Unix listener and init-held token.
- Log cursor advancement preserves later per-channel positions, within-record resumes skip only the requested prefix, and gap resume advances to the next record. No cursor duplication/loss finding survived inspection. The real-VM tests exercise a command cursor, member cursors, rotations and the shared cap, but were not run here.
- The established socket relay's reset path aborts its socket and releases reader-room waits. D251 resolves the granted path inside the workload root using openat2 plus descriptor-based connect, avoiding a second path resolution. No path escape was found.
- Image request decoding checks exact channel VERSION 27 before reading request fields; the inspected length reader bounds input and reserves fallibly. Recent versions 24–27 cannot silently reinterpret a request through that entry point. ABI run tags inspected are distinct; unknown control frames are refused.
- At head, `workload/capture.rs:157–159` accepts `SIGNALS_END` as `Served::RelayEnded`, preserving the `02a03edf2` fix: that scope ends without calling `end_guest(boundary)`. The unannounced/refused control path still ends the boundary and retains why.
- Retaining a held MCP connection after its first bytes is consistent with D254's stated bounded first-read behavior; its stale-before-first-byte case is separately handled.

## Not covered

- **No live reproductions or tests were performed**, including the catching tests above, existing managed_output/api_image tests, policy script, platform matrix, fd/thread measurements, mutation, E2E, chaos or profiling. Findings are source/state proofs requiring ROOT's live validation before fixes; none is claimed reproduced or passing under R09/R10.
- The remaining cause of ROOT's intermittent KVM failure is not established. M563 already records the `SIGNALS_END` refusal fixed by `02a03edf2`, while explicitly leaving the underlying init relay failure open. A further unverified lead is fatal exits bypassing pending ControlQueue flushing before `say_signals_end` (`init/run.rs:2328,5985`), whose nonblocking write ignores partial/zero progress. No reachable byte-level reproduction was established, so this is not a numbered finding or claimed explanation.
- Real Windows reset/linger behavior, HVF/KVM/WHP cut propagation, controller replacement under live traffic, guest compromise, and actual seven-target execution remain unverified. Source ownership checks are not a claim of a complete security proof.
- No performance ratio, allocation profile, resource-return measurement, or bounded-history measurement was obtained. Existing evidence records were read as historical evidence, never treated as passing checks of this review.
