# waits adversarial review — ranked P0–P5

Review date: 2026-11-13. Filed at the user's request; notification to Shards is the user's next step.

POLICY_READ: role **adversarial reviewer**, area **waits**; coordinator of three scoped read-only review lanes. Head `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`; base `79caf2d743d4c3c917f207bd650655a523847bf4`. `origin/main` resolved to the same head when read.

| Policy document | SHA-256 |
|---|---|
| AGENTS.md | `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41` |
| docs/development-rules.md | `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3` |
| docs/development-evidence.md | `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63` |
| CLAUDE.md | `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010` |

Read the binding brief first, all four policy documents, D180/D185/D209/D215/D222, and `docs/audit/2026-11-09_picked-constants.md`. All delegated lanes returned matching policy readbacks before review. Reviewed source and relevant changes in `79caf2d74...HEAD`. No production or test code changed; only this Markdown report was added at the user's request; no builds, tests, daemons, VMs, Docker, profiling, benchmarks or policy-check script ran. This artifact records review only. Evidence below is **source-verified call/state traces, not live reproductions**. R09/R10 runtime confirmation remains ROOT's under the brief's execution prohibition. No implementation or readiness claim is made.

Priority mapping used: P0 critical, P1 high, P2 medium, P5 low; no P3/P4 findings assigned. **Counts: P0 0; P1 1; P2 11; P3 0; P4 0; P5 0** (critical 0, high 1, medium 11, low 0). The fixed-constants inventory is one grouped R03 finding, with each location listed separately.

## Findings, highest severity first

### waits-01 — P1 / high — A second wall-clock verdict can OOM-kill a member while its authoritative wait is progressing

**Location:** `crates/shards/src/daemon/groups.rs:914`, consequences at `:765` and `:800`.

**Defect:** `overdue` compares `Word.since` to W* without consulting the active word's progress or host hold, then treats an unanswered reclamation as a completed pass that gave nothing.

**Scenario:** On macOS or Windows, a prior ROOM/RECLAIM teaches W*=1 ms. A group exceeds `memory.max`; its next word takes 3 ms while the VM runs, or is held runnable by its host. `tell_group_room` uses `Wait::WhileRuns(pid, &self.answers.room)` (`:986–991`), and `give_back_of` likewise uses the reclaim record (`daemon/reclaim.rs:214`). That judge correctly continues. Independently, the groups thread wakes at the word's elapsed bound, executes `record.stalled(word.since, now)`, changes `Given::Asked` to `Given::Nothing`, and, if all members are so marked and memory is still over max, records OOM and signals 9 to the largest (`:765–801`). Its answer has not yet arrived. Time waiting in `give_after` before this round's reclaim even starts is also included (`:853–856`).

**Evidence / refutation:** D183 says words are “judged as D215's judge judges a wait” (`architecture.md:7682`); D215 extends on progress/host hold. `Word.since` changes only at word transitions (`:822`, `:869`, `:889`), never when the inner judge observes progress. D183 also describes the outside `Record::stalled` timer; that documents the mismatch but does not make an unfinished pass a pass that gave nothing. A hard memory maximum alone does not prove this pass completed unsuccessfully. The separate thread's progress judge cannot protect against this parallel verdict.

**Confidence:** high, direct code/state trace; live reproduction unrun. **Axis:** Spec/R03/R04/R05.

**Fix direction:** Make the active word's authoritative judge/outcome report to the groups thread. Mark Nothing on an actual terminal refusal/end, accounting for host holds and the wait behind host-pressure reclaim.

**Unrun regression:** Prime W*; keep a group's member visibly working beyond W*, then return a positive give-back. Assert no OOM record or signal before that answer. Repeat with host hold and with `give_after` blocked behind another reclaim. Restoring wall-only `overdue` should fail these cases.

### waits-02 — P2 / medium — The first test-party observation can refuse a process before any CPU interval was observed

**Location:** `crates/shards/tests/common/mod.rs:3217`, `:3347`, `:3417`.

**Defect:** A newly discovered live process leaves the observation at Still, and the wait can immediately submit that Still to a judge whose whole first window has already elapsed.

**Scenario:** A call site's learned W* is shorter than the delay before its first look (e.g. a previous fast answer taught microseconds and the caller first polls milliseconds later). `Wait::on` starts the judge with an empty `Party.watched`; `Party::look` inserts `Watch::of` and sets `live=true`, then `continue`s without reading a CPU delta or recognizing its stopped/held state (`:3217–3226`). `Wait::look` submits Still at `:3417–3423` if its pre-observation `now` is past due. A busy or stopped party is refused with “its last progress its start,” although no interval of that party was judged.

**Evidence / refutation:** D209 requires a whole window without progress/hold; its text also admits the first-poll gap. `live=true` prevents Gone but does not prevent Still. Judge look-resolution changes cannot create the missing initial baseline. The initial `Watch` stores CPU/state but this branch never counts either as evidence or defers the first verdict.

**Confidence:** high, source trace. **Axis:** Spec.

**Fix direction:** Baseline members before starting their judgment window, or represent newly observed/unknown intervals explicitly and require a whole observed window before refusing.

**Unrun regression:** Teach a short W*, start a spinner (and separately a stopped child), delay the first `Wait::look(false)` past W*, and assert the first observation does not refuse. Then verify an actually observed still interval can refuse.

### waits-03 — P2 / medium — Already-satisfied test states teach tiny W* records

**Location:** `crates/shards/tests/common/mod.rs:3366`; concrete callers `:2382`, `:4872`; `crates/shards/tests/warm.rs:610`.

**Defect:** `Wait::answered` teaches elapsed time without checking whether the caller ever waited for an unavailable answer.

**Scenario:** `warm.rs::until_ended` creates `Patience::own`, finds stdout already ended on its first check, skips `holds`, then calls `done` (`:610–615`). Patient starts Holds, so `done` teaches a positive nanosecond/microsecond record through `Wait::answered`. The next genuinely delayed end from that call site is judged by that no-wait W*. Likewise, `Vm::wait_lines` teaches a matching line already queued before the wait began (`common/mod.rs:4865–4873`).

**Evidence / refutation:** D185's request-rest fix and D209's READY fix explicitly exclude already-present answers because they waited on no party. `Record::teach` rejects only exactly zero (`api/src/judge.rs`), while `answered` inevitably spends positive local time. Max aggregation preserves a prior large W*, but a cold call site is poisoned by the first already-satisfied case. This is separate from both first-look baselining and look resolution.

**Confidence:** high, source trace. **Axis:** Spec/R03.

**Fix direction:** Track whether the answer was initially unavailable and genuinely awaited; teach only such waits. Probe buffered channel/socket/state readiness before opening a learning interval.

**Unrun regression:** At one call site, satisfy a state before constructing its Patience, call done without holds, and require `Seen::Cold`. Repeat with a prequeued matching guest line. Then supply a genuine delayed answer and require learning from that interval.

### waits-04 — P2 / medium — A FIFO reader's death teaches a successful take

**Location:** `crates/ipc/src/slots/fifo.rs:123`; consumer `crates/shards/src/daemon/slots.rs:769`.

**Defect:** The large-write readiness branch emits `Wait::Ended` before proving that any bytes could be transferred, so disconnect wakes become success samples.

**Scenario:** Fill a FIFO, leave more than PIPE_BUF to write, and close its sole reader without reading. After WouldBlock, `write_all_waiting` emits Began, calls `await_room`, then immediately emits Ended (`:115–123`). `await_fifo_room` deliberately wakes on reader end too; Linux returns Ok for any positive poll, including POLLERR (`ipc/src/unix.rs:809–810`), and the next write distinguishes it. But `Living::answer` already calls `Seen::taken(took)` before that next write fails EPIPE. A cold take record is learned from no successful take, potentially seeding a tiny shutdown patience for subsequent live slow readers.

**Evidence / refutation:** The function promises “A wait that fails, its reader gone, is told no end” (`fifo.rs:106–107`). D185 excludes disconnect completions explicitly in its Windows twin. The <=PIPE_BUF branch correctly samples only after successful `write_all`; the defect is the >PIPE_BUF readiness branch. Clearing `live.waiting` after failure does not undo the sample.

**Confidence:** high. **Axis:** Spec/R03/R04.

**Fix direction:** Keep the take pending across readiness and emit Ended only after a write actually transfers bytes; failed writes teach nothing.

**Unrun regression:** Fill FIFO; in the Began callback close its only reader. Require BrokenPipe, no Ended callback, and Seen still cold. Positive twin drains bytes and learns a take.

### waits-05 — P2 / medium — Buffered registry bytes still teach fake network silences

**Location:** `crates/registry/src/http.rs:2008`; `:2049`; `crates/registry/src/patience.rs:122`.

**Defect:** Body reads time and learn every positive read, even when BufReader already held all its bytes and no peer was waited on.

**Scenario:** A response's head read brings its body into `conn.io`. `Body::read` starts an Instant, copies buffered bytes through `read_framed`, and unconditionally records elapsed as a silence (`:2000–2009`). A copy/deschedule taking >=1 µs passes `sample` and seeds a cold silence bound. A subsequent genuine millisecond network gap is then cut against local copy time. In RanOut state a buffered read exceeding the old sample can even re-arm a timeout mid-resume (`:2014–2021`).

**Evidence / refutation:** D222 explicitly excludes already-buffered reads. The zero-wait fix rejects only `as_micros()==0`, not buffered operations. Existing `a_wait_of_nothing_is_no_sample_and_no_bound` asserts only `!=Some(0)`, allowing positive fake samples. A prior larger maximum cannot shrink; vulnerable cases are cold records or post-run-out rearming. Exact runtime harm depends on socket timeout rounding.

**Confidence:** high that buffered work is learned; medium for any particular workload's resulting cut. **Axis:** Spec/R03.

**Fix direction:** Measure actual unready-to-ready I/O waits, excluding already-available buffer/TLS/socket bytes; a time threshold cannot establish readiness.

**Unrun regression:** Prefill Body's BufReader, gate execution after the timing begins without performing socket I/O, then finish the copy. Require no silence sample. Repeat in RanOut and require no rearming. Assert None, not merely “not zero.”

### waits-06 — P2 / medium — Retry-After exceeding a trouble budget is falsely learned as a trouble passed

**Location:** `crates/registry/src/registry.rs:1810`, `:1771`, caller `:860`.

**Defect:** The named-wait budget refusal returns None without setting either run-out flag, so its caller learns the refused trouble as passed and reuses the same budget next time.

**Scenario:** A path passed a 300 ms trouble earlier. A new request returns `503 Retry-After: 1` in 10 ms. `wait` returns None because after exceeds left. The request loop calls `trouble.over` and returns the failure; over sees ran_out=false and invokes `http.passed`. `Paths::passed` increments troubles and clears the run-out state (`patience.rs:513–519`). Subsequent requests remain bounded to 300 ms instead of following liveness after this run-out.

**Evidence / refutation:** The elapsed-budget branch correctly sets both `self.ran_out` and `http.trouble_ran_out` (`:1800–1803`); the named-wait branch does neither. D222 says named waits past the budget give up, a run-out learns nothing, and the next trouble follows liveness until one passes. The immediate caller really does call over for the returned failure, so no later repair is present.

**Confidence:** high. **Axis:** Spec/R04.

**Fix direction:** Mark both run-out states on every budget-driven refusal before returning None.

**Unrun regression:** Prime 300 ms budget; call wait with Retry-After 1 s; invoke over as production does. Require unchanged troubles count and `budget()==None`, then verify a later trouble can pass beyond 300 ms and teach its actual length.

### waits-07 — P2 / medium — The first successful dial suppresses restored route trouble and handshake records

**Location:** `crates/registry/src/patience.rs:398`; production order `crates/registry/src/http.rs:945`, `:952`, `:997`.

**Defect:** A TCP sample marks the whole path changed before its route record is loaded, causing restoration to import only answer/silence maxima and drop unrelated persisted trouble/handshake state.

**Scenario:** A routed links record has a 300 ms trouble budget and a handshake estimator from a 2 s TLS handshake. A fresh client has no route yet. `connect` calls `dial`; `dial` calls learned/waited and sets `p.changed=true` before returning the TCP stream. Only then does connect call `paths.located(local.ip())`. Located reads the correct routed record, but learn's changed branch copies only `slowest_answer_us` and `slowest_silence_us` (`:398–400`). The trouble is cold again, potentially retried forever, and TLS uses the cold 1 s estimator instead of the previously learned one. Fresh CLI processes repeat the failure to restore.

**Evidence / refutation:** D222 promises records are read again once a connection says its route. This is not preserving a newer value of the same field: a connect changed no handshake or trouble value. Later connections do not reload the unchanged route. The existing persistence test locates before taking a connect sample, reversing production order.

**Confidence:** high. **Axis:** Spec/R03/R05.

**Fix direction:** Merge independent fields according to their actual changes, maintaining the three-way persistence base, or register the route before teaching its connect.

**Unrun regression:** Persist routed trouble_us=300000 and a 2 s first handshake sample; instantiate fresh Paths, call waited(Connect,10 ms), then located in actual production order. Require budget 300 ms and handshake timeout 6 s, rather than None and 1 s.

### waits-08 — P2 / medium — An unreaped daemon zombie keeps startup waiting forever

**Location:** `crates/ipc/src/unix.rs:1167`, `crates/ipc/src/lib.rs:134`, `crates/shards/src/cli/client.rs:807`.

**Defect:** Unix Named liveness checks process existence and start identity, both of which still hold after an unreaped process has exited.

**Scenario:** A Linux daemon reports startup then dies before offering slots; its supervisor/subreaper leaves it unreaped. `kill(pid,0)` succeeds and `/proc/PID/stat` starttime still matches. The client has started=true, repeatedly finds NoDaemon, but `daemon_lives` is true, so the daemon-ended branch never runs and the loop sleeps/retries forever (`client.rs:807–822`). A stale stopping record has the same problem.

**Evidence / refutation:** D185 follows the daemon only while it lives. Start matching prevents reuse but does not detect exit. Windows' twin checks zero-time WaitForSingleObject. A new daemon could clear stale records, but this loop deliberately never starts another after started=true. Prompt reaping in normal cases reduces frequency, not the failure under a legitimate deferred-reaping supervisor.

**Confidence:** high for Linux predicate/loop; exact supervised startup reproduction unrun. macOS zombie behavior not established here. **Axis:** Spec/R04/R11.

**Fix direction:** Observe exit/zombie state as well as identity, or use an identity-bound exit notification. Exit must terminate liveness before reaping.

**Unrun regression:** Keep an exited child unreaped after it writes PID/start. Require Named::lives, daemon_lives and exiting to be false. Add startup-ready-then-exit/no-slots fixture and require a terminal daemon-ended result.

### waits-09 — P2 / medium — Test parties adopt a reused daemon PID and count zombie observations as ongoing progress

**Location:** `crates/shards/tests/common/mod.rs:3140`, `:3231`; `crates/api/src/progress.rs:209`.

**Defect:** The harness discards the daemon's recorded start identity, and treats repeated ending observations as a live progressing party rather than a terminal process.

**Scenario A:** A stale daemon.pid contains `PID OLD_START`; PID now belongs to an unrelated busy process. `Party::daemon` parses only the first word, then `Followed::of_pid` adopts that process's current start as correct. Patience follows unrelated CPU forever. D185's production Named check would reject it.

**Scenario B:** On macOS a test's child daemon exits and remains unreaped while its test waits for a state. proc_pid_rusage continues to provide exit time; `found(ending=true)` always returns Progressed. `Party::look` counts every such observation as live and as daemon progress (`:3231–3234`), so no-party-gone handling never ends this wait. Ending must win over a racing refusal for a client whose exit is being delivered, but is not perpetual liveness for a state waiter with no such exit consumer.

**Evidence / refutation:** Modern daemon files include PID and START. The shared Watch correctly checks the start identity it was given; the harness gives it the new process's identity. The shared ending priority is appropriate for production's reap/wait path, but this caller has no equivalent terminal transition. D209 explicitly requires a crashed daemon's stale pid file to be no live party.

**Confidence:** high for PID identity loss; high for the source-level repeated-ending classification, with macOS zombie reproduction unrun. **Axis:** Spec/R04.

**Fix direction:** Retain and validate Named's start identity. Represent exited/ending separately from ordinary progress at this caller and terminate state-wait liveness once exit is authoritative, preserving the final state recheck.

**Unrun regression:** Write current spinner PID with deliberately wrong start to a home's daemon.pid and require Patience's party gone. Separately leave a macOS daemon child unreaped after exit and require a terminal state wait after its final recheck.

### waits-10 — P2 / medium — A cold command wait can hang after its client exits while another process holds output open

**Location:** `crates/shards/tests/common/mod.rs:1999`, `:2010`, `:2690`, `:2715`; `crates/api/src/judge.rs` Judge::begin/found.

**Defect:** The same cold Judge is retained for stream draining after the client's answer, so no verdict ever ends a drain whose writer remains alive but silent.

**Scenario:** First invocation of a call site runs a process that starts a background descendant inheriting stdout/stderr, then exits. try_wait yields status and `wait.answered` teaches W*, but the Judge began cold and its judged flag remains false by design. Output EOF never arrives; loops return only on both EOFs or Err(stall). Every cold look remains Ok even if party lookup reports Gone, so the completed client's output drain hangs until the CI job ends. Descendants reparented after exit are also no longer reachable by the reconstructed client tree.

**Evidence / refutation:** Cold liveness is documented as the client's end; that end already occurred. Keeping a wait that began cold unjudged is correct during the original operation, but not a substitute for a drain lifecycle after that operation answered. The explicit `(Err(_), Some(status))` escape exists but cannot fire for this Judge. There is no separate drain judge or authoritative holder closure.

**Confidence:** high. **Axis:** Spec/R04/R05.

**Fix direction:** Give post-exit drain its own explicit lifecycle, observations and learning policy; end it from the actual stream-holder state or a properly judged drain, rather than reusing an eternally cold client wait.

**Unrun regression:** Fresh call-site kind; launch a stand-in which spawns a silent inherited-output holder then exits. Assert the helper returns/reports held output after client end; cleanup holder through a test gate. Repeat with learned kind.

### waits-11 — P2 / medium — Concurrent record writers can make the persisted slowest answer shrink

**Location:** `crates/shards/tests/common/mod.rs:3006`.

**Defect:** `WaitKind::keep` performs read/max/write/rename without interprocess exclusion, so atomic replacement does not make the maximum merge atomic.

**Scenario:** Two test processes use the same persisted kind. Both read old W*=1; A snapshots max(1,10)=10, B snapshots max(1,100)=100. B renames 100 first; A renames 10 last. Next run reloads 10, losing an actual 100 answer and potentially refusing its next valid quiet interval. Distinct PID temporary names prevent partial files but do not serialize the merge. Same-process concurrent keep calls additionally share a temporary pathname.

**Evidence / refutation:** D209 promises an answer merges with what another process kept. `keep` reads once, serializes its in-memory snapshot and renames (`:3007–3026`); no file lock/CAS surrounds it. The per-process KINDS mutex is not held here and cannot protect another process. Max is commutative only when each update includes all prior committed samples.

**Confidence:** high, explicit interleaving. **Axis:** Spec/R03/R05.

**Fix direction:** Serialize the complete read/merge/replace transaction across processes, with distinct per-operation temporary paths; preserve both W* and R* under concurrent writers.

**Unrun regression:** Two processes with barriers after loading old data; let the long sample rename, then the short sample rename. Require reloaded W* to remain the long sample. Mutation without transaction exclusion should deterministically lose it.

### waits-12 — P2 / medium — Chosen production timeouts, retries, windows and polls remain

**Defect:** The paths below still choose runtime waits/retry behavior from fixed figures rather than online data or justified protocol bounds. They are source-confirmed R03 violations; merely mentioning a default or documenting unfinished work does not derive it.

| Head file:line | Fixed value / effect |
|---|---|
| `crates/shards/src/daemon.rs:225` | TAKE_TIMEOUT 10 s; still bounds run-socket writes (`:974`), link read/write handoffs (`:7720–7722`, `:7788–7790`), door writes and connect/take exchange (`:8948`, `:8966`). |
| `crates/shards/src/daemon.rs:236` | TAKEOVER 20 s for lock holder that neither offers slots nor is marked exiting; used `:2089`, `:2121`. |
| `crates/shards/src/daemon.rs:2125` | 10 ms lock polling. |
| `crates/shards/src/daemon.rs:145` | HANDOFF_TRIES 3; run startup stops trying warm VMs after the fixed count (`:4458`). |
| `crates/shards/src/daemon.rs:8807` | 1 s wait for process-status diagnostics, with fixed 10 ms polls at `:8818`. |
| `crates/shards/src/daemon.rs:252` | UNWATCHED 250 ms file-watch fallback; comment expressly calls it picked and owed. |
| `crates/shards/src/cli/client.rs:822` | 1 ms readiness polling for a started daemon with no slots. |
| `crates/ipc/src/slots/fifo.rs:763` | macOS list's 250,000,000 ns fallback where its process watch is absent; D185 explicitly lists it as still picked. |
| `crates/shards/src/daemon/slots.rs:29` | SWEEP 60 s; abandoned taken slot removed after its second sweep. D185 repeats the minute but derives none. |
| `crates/shards/src/daemon/slots.rs:706` | Fixed 60 s window for max-client logging. |
| `crates/shards/src/vm_run.rs:2333` | Joiner end reports waited for 5 s via `warm::await_joiners`; joiner still reporting after that loses its holder. |
| `crates/shards/src/vm_run.rs:2346` | Relay outcome recv_timeout 5 s after VM end. |
| `crates/init/src/hostbind.rs:67` | ANY_PORT_TRIES 8; port-zero bind/relay retries stop after chosen count (`:1190`, `:1249`, `:1415`; decrement `:1349`). |
| `crates/registry/src/registry.rs:29` | Five throttling jitter ranges [50,100,200,400,800] ms; absent accepted allowance window, sixth 429 is refused. PM M112's single ECR experiment does not derive these online. |
| `crates/registry/src/registry.rs:1690` | 800 ms floor in `waited.max(THROTTLED.last()...)` past that schedule. |
| `crates/registry/src/registry.rs:38` | WINDOW_MOST 60 s: a server-declared allowance window of 61 s falls back to the five-retry schedule (`:1684`). |
| `crates/registry/src/http.rs:48` | RACE 300 ms before launching next address; D222 explicitly admits this upstream default still needs derivation. |
| `crates/registry/src/http.rs:47` | CONNECT 30 s for nonlearning production clients, retained from Go/containerd. |
| `crates/registry/src/http.rs:51` | HANDSHAKE 10 s for nonlearning production clients, retained from Go. |
| `crates/registry/src/http.rs:53` | HEAD 30 s; unsafe/nonlookup requests still take HeadWait::Patience (`:801–810`), including learning registry clients. |
| `crates/registry/src/http.rs:54` | STALL 30 s; also bounds learning registry request writes (`:977`, `:813`). |

**Concrete failures:** A healthy VM whose host holds its socket reader beyond 10 s fails a handoff/write despite liveness. Three failed warm candidates prevent trying an available fourth. Eight host-chosen ephemeral ports colliding in the guest refuse bind despite other usable ports. A registry whose bodiless POST finishes after 31 s is cut at HEAD, or one whose 61 s allowance would clear after 2 s is refused after fixed retries. A healthy alternate address is launched only after chosen 300 ms. The fixed polls additionally keep waking for readiness without being derived from their running look cost. The log window is policy-only, not asserted to lose data.

**Evidence / refutation:** R03 explicitly rejects arbitrary timeouts/retry counts/floors/windows/polls and says upstream defaults or one measurement are insufficient. D185 admits its fallback gaps; D222 preserves Go defaults and the throttle schedule but supplies no sufficient derivation. STOP_GRACE's 10 s is a user-visible Compose/Docker input semantic and is excluded; RFC6298 estimator parameters, QUIC's RFC9002 cold RTT and numeric representation limits are also excluded. No claim is made that all fixed numeric literals in the repository were inventoried.

**Confidence:** high for presence, uses and R03 mismatch; example runtime failures unrun. **Axis:** Standards/R03; behavior also Spec.

**Fix direction:** Use authoritative readiness/completion notifications or local online wait learning with explicit cold/run-out/cancel behavior. Retry choices should follow actual resource/authority/progress state. Derive race scheduling and throttle handling from the actual path, rather than retaining these schedules. The group is intentionally review-only; no replacement constants proposed.

**Unrun regression:** Delay an otherwise live VM's take/write, supply a fourth usable warm candidate, force the first eight ephemeral choices to collide, vary a registry's declared window across 60 s, and give a nonlookup answer beyond 30 s. Require behavior governed by liveness/progress and actual path state. A focused source audit should additionally reject the literal polls/windows without a cited sufficient bound.

## Checked and found sound

- `progress.rs::found` has the requested precedence: ending, stopped, ran, held, still. This is a source observation, not a portability pass. Record W* is max; R* is diagnostic; u64-saturated rates use u128 cross-products.
- Linux schedstat growth is process-thread-specific. Shared/root cgroup PSI is excluded from host-hold evidence; exclusive cgroups can contribute. Stop/frozen overrides CPU run-to-stop. macOS reads runnable time and per-thread states without requiring a task port.
- D185 request-rest training has an actual readiness probe and `waited` flag; already-buffered requests do not teach. The first rest byte removes the deadline, supporting requests larger than the FIFO. Cold requests follow client end or daemon cancellation.
- Immediate answer writes do not teach. The <=PIPE_BUF blocking-write branch teaches only after successful transfer. The large-readiness branch is waits-04.
- Modern production Named start comparison prevents reused-PID identity acceptance (but not zombie exit, waits-08). Again compares named daemon identity instead of a fixed RESTART count. The test-party parser loses that protection, waits-09.
- Registry zero/sub-µs samples and stored zero bounds are filtered; dial/handshake code rejects zero remaining time before socket timeout/connect_timeout calls. No further source-proven Duration overflow or zero-timeout defect was established in the examined paths. Buffered positive fake samples remain waits-05.
- Ordinary elapsed-budget Trouble refusal sets both run-out states; safe-lookup answer/silence run-outs have explicit state. D222 DNS/dial cancellation waits on events/Condvar rather than its former 100 ms poll.
- D180 explicitly distinguishes run-out/stall from peer death, follows the stalled connection with no window, and retains QUIC probing. That is the documented and examined state-machine intent, not a completed live transport audit.

## Not covered

- No live reproduction, builds, unit/E2E tests, faults, mutations, policy checker, profiling, benchmarks or cross-platform execution. All proposed tests remain unrun; nothing here is a passing validation gate. The report gives ROOT concrete fixtures to validate before fixes under R09/R10.
- The other agent's judge look-resolution fix is not reviewed or duplicated. No claim that waits-02/03/04/05 are its only independent false-stall paths.
- Full HTTP framing/auth/TLS/security, registry transfer concurrency, all named-pipe fault paths, all production constants repository-wide, and full D180 transport behavior under loss/freezes were not exhaustively audited.
- Test record scope is explicitly only OS/architecture/logical CPU count (`common/mod.rs:3033`), with no build or physical-host identity (`:2975–2978`). D209 deliberately permits sharing between CI-image runners. This **does not establish same-host learning**; records can cross machines/builds when target is restored. Recorded here as a known scope/authority concern rather than inventing a separate undisputed implementation requirement contrary to that explicit design choice. ROOT should reconcile it with the current host-local R03 ruling.
- DEFAULT_IDLE=900 s and the remaining warm-pool capacity/failure defaults deserve a full D26 policy review; this report does not claim to have completed that separate capacity-policy audit. Hostbind's Linux-conntrack-derived UDP aging and per-turn BUDGET copy were inspected but not proved to violate its intended parity semantics. Numeric OS/protocol/unit bounds were not automatically labeled picked constants.
- Cold waits whose live party never answers, spinners counted as progress, a Patience affected by another test's own VM, and helper-thread/exec polling work contaminating a party are documented design gaps; no new live proof was obtained here.
