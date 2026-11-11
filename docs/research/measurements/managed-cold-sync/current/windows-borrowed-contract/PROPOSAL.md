# Native Windows borrowed contract: source preparation, no execution

This is a finite **source/interface proposal for ROOT review before builds or
actors**. No native test, strict lint, typecheck, fault campaign, VM or resource
attribution has run for this source. Rustfmt parsing is not a compiler gate.
The existing corrected borrowed connector is already main; this patch adds no
production connector, transport, deadline policy or cold synchronization fix.

POLICY_READ implementer: FULL current origin/main
`ca3ab10c151aa5539e757f1cc76029746ccbb561` AGENTS.md,
docs/development-rules.md R01–R16, docs/development-evidence.md, CLAUDE.md,
managed-runtime-plan.md and updated `/Users/adalundhe/g121-handoff.md` reread.
Rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
The research skill's primary-source Markdown workflow was applied directly;
the explicit no-child instruction overrides its background-agent suggestion.

Only owned source: six cfg(test,windows) lines in `crates/api/src/client.rs`,
NEW `crates/api/src/client/windows_borrowed.rs`, and this dedicated evidence
directory. Branch `p/managed-windows-borrowed-contract-v1` starts exact ca3 in
the existing worktree. Previous cold/API/source/raw refs remain frozen, including
`5c20f46394a7505361879aa538ae301b63857b1f` and its failed-overall r612 receipt.
The three old untracked step9 files remain untouched. No new worktree/full copy.

## Existing seam and missing proof

Main ca3's adopted API is the same corrected 33df contract. It establishes
`Ready` for borrowed zero, one `Until` before native identity for positive
patience, and Bounds for MAX/overflow before identity or dial. Owning
`Guest::connect` retains its separate zero one-dial/await-answer and MAX no-clock
semantics. The [source record](source-identities.json) hashes these exact callers,
the shared handshake and existing stream/afunix leaves. Removing ONLY the six
new cfg(test) lines recovers byte-identical ca3 client.rs. No obsolete Hex patch
is applied. The test patch is independent of adoption of the cold-sync candidate.

The frozen ec494 Windows record passed at f08:29 ordinary lib and4 simulator
tests, strict API/afunix lint0. That record explicitly excludes the Unix-only
public zero-withheld and native owning zero/MAX stand-ins. Its positive borrowed
identity names the test process. It does not establish these new held-child cases,
current-source execution, handle return or new passing ignore counts.

The fixture calls real `connect_borrowed`, `Guest::connect/identity/stop/wait_for`,
`borrowed_windows::Held`, `created`, `start_threads`, `Exit` and the existing
shards-afunix listener/stream. It initializes ONE actual `Shared` exactly as
main `start` does. No second State, lifecycle manager, Windows transport or
CONNECT/OK client exists. This is **not a public boot/start integration gate**:
the support child is started by the fixture, and the trusted ShardsGuest is
assembled at that existing private seam. ROOT must approve that boundary before
execution; a real carried VM remains outside this batch.

## Exact observation hook

The only inserted call is immediately after existing wait selection and before
the actual existing `Held::find`:

```rust
#[cfg(all(test, windows))]
windows_borrowed_contract::before_identity(began, wait);
```

The other four lines register the Windows test module. Non-test code has no hook
or added storage. An unarmed test returns immediately. The armed, thread-local
gate is consumed once and carries only the original `began`/`HandshakeWait` and
a test receipt channel; it never retains a Guest, guard, handle or socket.
`Observe` sends that exact clock. The single `Expire` case waits on a finite
channel silence ending at the SAME original Until, then lets the **real native
identity path run**. It expects the subsequent existing Dial deadline check to
refuse without a peer. It does not pretend native identity itself was slow or
trace GetProcessTimes duration. The guard clears the test gate during unwinding.

Alternatives: a wall-clock-only bracket cannot identify when identity consumed
the budget; a separate fake connector would not exercise this API; a native
identity delay/fault actor or transport shim expands authority. This one cfg(test)
observation point supplies the exact original clock without any production
algorithm or error change. Its choice and live proof are ROOT-review pending.

## Proposed finite selectors and witnesses

Prefix `client::windows_borrowed_contract::`; each of these eight source selectors
is ignored and requires exact ROOT selection. The ninth ignored selector,
`held_child`, is support code ONLY, never an additional passing parent case.
Source enumeration is not actual libtest list/pass/ignore evidence.

| Case suffix | Required future observation; missing witness is failure |
| --- | --- |
| `positive_borrowed_drop_keeps_owner_alive` | One exact CONNECT, OK plus binary payload preserved, borrowed stream drop gives peer EOF, muxer joined, native owner still alive and same creation identity. |
| `zero_reports_actual_pending_phase_and_peer_bytes` | No accept/reply worker exists before Ready returns. Exact typed WouldBlock phase/cause; then one actual accept observation, exact committed CONNECT prefix and EOF if accepted. No answer can have been fabricated. |
| `max_is_bounds_before_mismatched_identity_or_dial` | A deliberately mismatched start for the real held child still yields Bounds(patience), does not reach the pre-identity hook, and has no queued peer. No Process error is accepted as success. |
| `positive_deadline_is_established_before_identity` | Exact Until equals original began+positive patience; it expires at the pre-identity gate; real identity then succeeds and existing Dial returns typed Expired; listener has no peer. |
| `withheld_reply_expires_before_peer_release` | Actual full CONNECT, no reply, exact Read Expired recorded BEFORE the permit to release peer; peer then observes EOF and joins. |
| `dribbled_reply_keeps_the_original_deadline` | Actual request and at least one peer byte written before the original Until; scheduled partial OK remains incomplete at Until; exact Read Expired, raw peer write outcomes and join retained. A peer write is not a traced client read. |
| `owning_zero_still_awaits_one_answer` | SAME owning implementation awaits a deliberately delayed OK for its one dial, then returns a stream; peer EOF/join and native owner alive. Borrowed Ready semantics are not substituted. |
| `owning_max_still_has_no_clock` | SAME owning implementation succeeds after the fixed delayed OK with MAX; no finite production patience is substituted. This positive case does not prove unlimited retry behavior. |

For zero, Windows may return a pending **Dial**, or connect may complete and reach
Write/Read. The fixture does not require Unix's immediate full request. A Read
pending result requires the full peer-visible request; Write requires an exact
short prefix; Dial permits no accepted peer or an accepted peer with no bytes.
Unexpected error/phase, an extra accepted peer, absent EOF where required, or a
missing prefix is a failed/coverage-miss case, not a skip or retry opportunity.

`WB_READY_PENDING`, `WB_READY_NO_ACCEPT`, `WB_ACCEPTED_AFTER_READY`, `WB_REQUEST`,
`WB_POLL`, `WB_PEER_EOF`, `WB_CALL`, `WB_MAX` and `WB_DRIP_*` retain the actual
typed phase, code (including None), returned flags and bytes. A single
accept-WouldBlock result **does not prove never dial**. The MAX ordering proof
also needs the exact Bounds result and unchanged source ordering. Dial WouldBlock
from the current afunix branch may have code=None after its WSAPoll(0) returns0;
do not fabricate WSAEWOULDBLOCK=10035 as the returned raw code. There is no syscall
trace/call-count hook or whole-time-window absence claim.

## Deadline ownership and proposed outer execution interface

No execution has been submitted. Candidate runner is native Jupiter AMD64
`f766f09294f2e17e3b8a631ffd4e6a4b`, NOT paused Windows ARM.
Proposed admission reuses the prior Windows scope:2CPU/4GiB/12GiB, j2,
compile180s, each case15s, outer900s. These are a proposal for ROOT's execution
gate, not admission or proof of an enforced RSS/disk quota. Use NEW private
source/CARGO_HOME/target/raw/frozen and a NEW short TMP/TEMP; no ROOT caches or
old execution roots. Respect normal headroom/approval service, no fallback route.

Future parent invocation, after ROOT approves exact source/supervisor:

```text
SHARDS_WINDOWS_BORROWED_MS=15000
api-unit.exe client::windows_borrowed_contract::<one-suffix> --exact --ignored --test-threads=1 --nocapture
```

The case creates one absolute Instant before any fixture setup/child spawn.
All peer readiness, control-channel waits, normal cleanup and fallback native
child waits use what remains of it. ROOT's external case deadline starts before
launch and is not reset at readiness. `SHARDS_WINDOWS_BORROWED_MS` is an explicit
test input; the fixture has no default. Positive borrowed calls use the existing
`conformance::UNSERVED_PATIENCE`200ms, a test input, not a production replacement.
The actual original began/Until is captured at the one test hook and checked
exactly; no timing tolerance or new per-I/O deadline is substituted.

Owning zero/MAX keep their actual production wait modes. The peer answer release
is one absolute instant computed before calling, using that same200ms input.
Dribble ticks are original-began+index*(patience/2), five protocol bytes; final
newline would be at400ms, past the original200ms Until. The peer still uses the
original case deadline for I/O. Missed pre-expiry byte/release witnesses fail;
there is no adaptive sleep, enlarged budget, warmup or runtime repetition.

## Owned handles, join/cleanup order and forbidden effects

1. ROOT's existing .NET Process wrapper owns the parent test command's process
   handle and raw stdout/stderr capture streams. Extend its held-child identity
   observation only as reviewed by ROOT: read `WB_READY case=... pid=... started=...`,
   hold the native child process, verify its creation FILETIME, and retain its
   exit observation BEFORE writing one `R` byte to the parent case's stdin.
   A PID-only observation or failure to hold/verify it is not an execution pass.
2. Fixture `ChildOwner` owns the exact std Child through Arc<Mutex<Child>> and its
   unique stdin writer. The support child is this same frozen API test binary,
   invoked at the exact helper selector with its private environment flag. It
   creates no descendants, handles no network and waits for one X byte or EOF.
   Closing the writer on parent exit is its lifetime release; the helper has no
   independent clock, so ROOT's finite outer supervisor is mandatory. No host Job
   is created, assigned, configured or counted. Ambient inherited Jobs are not
   inspected. Inheritance/whole-population cleanup
   is not inferred merely from source or a closed parent pipe.
3. `borrowed_windows::Held` owns only a query/synchronize handle to that exact
   PID/start; each real borrowed call independently obtains/closes its existing
   own handle. Production `Exit::of` duplicates a synchronize-capable Child
   handle for the actual watcher. The existing watcher waits that object and
   calls Child::wait before publishing exited. No borrowed object gains stop
   rights. Created identity is not authentication of the AF_UNIX peer.
4. Every case observes native owner alive after borrowed success/drop or refusal.
   Peer threads are joined in normal successful cases before explicit owner stop.
   Actual `Guest::stop`, finite `wait_for` Some(Stopped), Held::check Exited and
   Child::try_wait Some(status) are separate witnesses. The cached child status
   after production wait is not Unix ECHILD and is not described as a second reap.
5. Drop releases Guest, held observer, child control/handle, listener and only
   the fixture-owned PrivateDir. It uses the existing exclusive-create path,
   never TempDir's stale-directory sweep. Directory absence is asserted. The fallback Child
   guard closes its writer, kills ONLY its still-held native Child if still
   observed running, waits on that same handle for remaining original time,
   and records actual status/kill/wait/error. Failure/timeout is retained as
   uncertain cleanup. It never scans/kills by PID, retries or extends a deadline.
6. ROOT retains both actual parent and stand-in native exit observations and
   closes its own observer/capture handles. A parent timeout/failure is failure,
   including any helper exit/cleanup uncertainty. No automatic rerun. Existing
   API watcher/drainer JoinHandles are discarded by production start_threads;
   they are NOT exposed/joined by this patch, and exited is not a proof of their
   final thread/handle destruction. Full handle-stability/resource attribution,
   enclosing allocation and process/kernel return remain separately UNKNOWN.

Synchronous OS calls, lock acquisition and scheduling are not hard real-time
bounded by these fixture deadlines. ROOT's external finite supervisor remains
mandatory, with timeout/unknown cleanup retained as a failed gate.

Forbidden effects: borrowed drop/refusal must not stop the owner; overflow must
not reach identity/dial; Ready must not await accept/reply; partial reply must not
renew Until; new helpers must not own a Job/VM/admin/security setting or affect
another process/path. No allocator refusal, native-error injection, forgotten
guard, worker-result hook, held borrowed campaign, profiler or handle-count probe.

## Primary identities, checks and R01–R16

[primary-identities.json](primary-identities.json) records exact bytes/hashes from
MicrosoftDocs sdk-api commit `08ed091870a25a4ddeffe0727789ca0164beadd2`, retrieved
2026-11-11 UTC. [connect](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-connect)
distinguishes nonblocking pending connect from completion;
[WSAPoll](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsapoll)
defines zero/finite/indefinite timeouts and returned error/readiness flags.
[WaitForSingleObject](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject)
requires the waited handle to stay open and distinguishes signaled/timeout/failure;
[GetProcessTimes](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes)
provides creation FILETIME through the held query handle. These contracts and
deployed source facts are not measurements of afunix.sys, identity latency or
native resource return. Source layout/header facts do not establish a runtime gate.

`checks.json` records only actual source/format/policy/diff checks. Restart cut
read-only policy requests before any batch edit/build/actor; their response was
unknown, not a test failure/pass. Initial focused rg/sed included sparse-missing
platform/afunix/ipc paths and returned absence diagnostics; exact current-main
files were then read with git show, without guessed source. No test result count
is assigned to any such read, formatting or preparation command.

The full staged `git diff --check` actually returned2 at `source.diff:17`:
that verbatim review patch contains a one-space blank **context** line. The
original patch is retained byte-identical (SHA256 in checks.json). A separate
staged check excluding ONLY this literal patch returned0. This does not turn the
full staged check into0 or waive a source lint/test. No Rust file has that
reported trailing-whitespace diagnostic.

| Rule | Concrete preparation result / owed proof |
| --- | --- |
| R01 | Non-test bytes unchanged after removing two cfg(test) blocks. Test assertions permitted. Compiler/lint/native paths UNRUN. |
| R02 | Exact adopted callers, prior Windows coverage limits and four pinned primary contracts; observation choice ROOT-review pending, no new production architecture. |
| R03 | Zero/MAX/protocol framing are existing contracts;200ms existing test input, remaining bounds explicit proposed ROOT inputs. No production sizing. |
| R04 | Real State/Tail/Shared, typed refusal/cause and typed test gate only. No duplicate lifecycle/terminal/error manager. |
| R05 | Existing notifications/held process/native readiness; no polling fallback, cache, warming, transport shim or result manager. |
| R06 | Eight finite focused contracts plus one non-descendant support child; not a broad corpus/audit/sweep. |
| R07 | Hook/precondition/handle and raw return versus inference limits explicit. Future strict lint not claimed. |
| R08 | No measured ratio or performance claim. |
| R09 | No new reproduced production defect and no production fix. Pending/overflow/deadline results remain future observations. |
| R10 | Exact review diff, base/source hashes, primary identities and command/check record; actual red/green/native evidence owed. r612 overall failure preserved at its original ref. |
| R11 | Windows-only preparation; no native Windows/WinARM/seven-target/VM claim. Mac support ARM64 only; paused WinARM untouched. |
| R12 | Existing shared handshake and Windows wrappers, no extra production binary/OS manager/crypto/root/security change. |
| R13 | Small cfg(test) call/registration and focused fixtures only; no adopted product path rewritten. |
| R14 | No benchmark/profile/allocator/handle-stability measurement or leak-free claim. |
| R15 | Implementer source-only authority; no build/actor/children/goals/mainmerge. ROOT owns execution approval, combined gates and resource attribution. |
| R16 | Same worktree, fresh branch, all old source/raw/frozen refs/holds/targets preserved; no cache deletion or headroom bypass. |

ROOT must review the exact hook/fixture/supervisor proposal, then assign any
finite native compile/list/strict-lint/case gate with actual admission. Live
Windows results, exact actual ignore/selection counts, borrowed/owning cleanup,
fault/resource attribution and combined integration remain unrun. This is a
checkpoint in the sustained lane, not lane completion.
