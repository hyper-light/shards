# Borrowed Windows handles: bounded component fixture

This directory exercises the explicitly unverified Windows handle-count
obligation using the real positive borrowed connector. It adds no production
fix or handle-stability threshold. Raw point counts do not establish leak
freedom, allocation behavior, peak use, kernel return or process-tree cleanup.

## Authority, source and ownership

POLICY_READ implementer `/root/terminal_order_fixture` initially read full
canonical `24687c98f8c507981d91eb887d511fa7f4e2b83a` AGENTS/rules/evidence,
CLAUDE, managed plan and updated handoff before new work. Original rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
ROOT approved the concrete private-child/caller/ownership design and primary
contracts before code. The prior terminal fixture commit/ref/artifacts
`6d0f699ee6c34def66ae6219e734abfd7b66272a` remain preserved.

During implementation ROOT assigned this lane bounded live Windows adversarial
correctness work under updated canonical
`cbd39284937d024640c56db6f8d8ef2bd9b0ee24`. Full new AGENTS, R01–R16,
development-evidence, current plan and full handoff were read before continuing;
rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Role is now **implementer + assigned adversarial tester** for these three owned
modes. Current R15 and ROOT's explicit assignment supersede older coordinator-only
paragraphs in the old source/handoff. Audits, profiling, benchmarking,
cross-lane integration validation and main merges remain coordinator-owned.

The existing exclusive `managed-output-endpoint` worktree was clean before
creating `p/native-borrowed-handles-v1` directly from signed API source
`3b3817eea489d107fb9820f0f46c4fe429709fa3`, without merging old ancestry.
Owned source is **only this new directory**. API/client/lib/stream/afunix files,
dependencies and locks in the working checkout are unchanged. The nowait API
writer owns those files; coordination explicitly restricted this fixture to
positive finite patience. The later zero/MAX extension is not composed or tested.

No children, goals, new full local worktree, local cargo target, host settings,
install, privilege, trust, crypto or paused Windows ARM activity. Linux had no
idle headroom; no Linux request bypassed it. Jupiter admitted 1 CPU / 1 GiB memory
/ 2 GiB disk in NEW private source/Cargo home/target paths under
`C:\Users\corph\native-borrowed-handles-3b-light-v1`. Admission is not a measured
peak or hard quota. Protected coordinator caches were never touched.

## Existing seam and states

[`focused.rs`](focused.rs) is a Windows test child of `client`, registered only by
the **unapplied** [`registration.patch`](registration.patch). This reuses the
actual private `created_handle(GetCurrentProcess())` producer and public
`connect_borrowed(&Identity, u32, Duration) -> Result<Box<dyn Stream>, Refusal>`.
A public integration-only test would duplicate the private identity producer.
No production testing interface or alternative connector/handshake is introduced.

Current-process PID/start and an owned AF_UNIX fake-muxer path are explicit
fixture inputs, not a VM identity or proof of peer authentication. Each real
connector call obtains the existing query/synchronize, non-inheritable held
process handle, checks creation/liveness, runs the shared production CONNECT/OK
exchange, rechecks, and drops the held handle. There is no pre-call Held warm-up.
Only the actual returned stream can escape; no Guest/adoption/watch/stop or
cross-process WSADuplicateSocket path is used.

The fixture has one listener, one fixed server thread and at most one accepted
connection. Each step is command → server step-ready receipt → actual positive
API call → caller release → server receipt → count. Bounded channels use one
handoff slot because only one operation can be in flight. A missing/full handoff
is an explicit failure, never a spin/retry. No periodic probe/cache/pool/manager.

| Native mode | Actual operation and required effect receipt |
| --- | --- |
| `success` | Real connector sends CONNECT, production parser accepts owned server OK, a one-byte challenge/answer passes, stream drops; server observes actual EOF, stops I/O and consumes its sole accepted wrapper into OwnedSocket/raw ownership for exactly one closesocket. Only close0 + successful exchange permits the operation sample. |
| `wrong-start` | Same actual PID with a checked distinct start. Requires typed DifferentStart with the real observed creation time. After result, server performs one nonblocking accept; only actual WouldBlock/WSAEWOULDBLOCK is a no-queued-dial receipt at that cut. Unexpected accepted sockets are actually closed and retained as a failed effect witness. |
| `withheld` | Server validates the real CONNECT then sends no reply bytes while waiting on a bounded release event. Requires actual typed Read expiry before caller releases; server then observes EOF and records actual close0 before counting. No sleep, dribble or hidden retry. |

The one-byte fixture challenge is not authentication. Port `u32::MAX` covers the
existing decimal protocol width. Fixed challenge bytes are fixture data; no
production default/capacity/policy literal is added.

## Counts, deadlines and cleanup

Count snapshots use actual `GetProcessHandleCount(GetCurrentProcess())`, preserving
raw BOOL, DWORD on success, or immediate GetLastError with **unknown** count on
failure. Zero is never substituted for failed observation. The first snapshot
is fixture-entry cold, before fixture setup/output; libtest/process startup
already happened. Further snapshots are setup, every acknowledged operation,
after operations, and actual post-server-join. All values remain unadjusted,
including runtime/Winsock first-use retention. No warm-up, subtraction, tolerance,
leak threshold or count-based functional pass is used. Serialization occurs
after teardown. Inheritance queries preserve raw flags/errors without rewriting
any security/inheritance setting.

The current-process pseudo handle is borrowed, never closed or converted into
OwnedHandle. The API's real process handle is RAII-owned internally. Accepted
socket close is observed explicitly after all peer I/O ends, using the existing
UnixStream → OwnedSocket conversion and IntoRawSocket ownership transfer.
closesocket is called once; failure records its actual return/native error,
stops further iterations and leaves resource state unknown until process exit.
No CloseHandle is called on a socket. Listener Drop's close return is
**unobserved**, always reported as null. EOF/drop/close0/join do not prove kernel
resource release or delivery of arbitrary earlier data.

Caller supplies positive repetition count and finite case/call/cleanup/outer
budgets. One original whole-case Instant starts before cold collection; an
explicit cleanup slice is reserved. Each operation's deadline is bounded by both
its supplied call budget and that original operation cut. The connector receives
remaining positive patience once. Fixture I/O uses the same original call cut;
cleanup receipts use a supplied cleanup budget clipped by the original whole
case. WSAPoll waits for actual readiness with positive rounded-up milliseconds;
input case bounds derive from its signed millisecond maximum. Interruptions or
unexpected readiness/error flags become evidence failures, not periodic retries.

Cleanup disconnects the owned command channel, waits only within the original
case/cleanup cut, and joins only after real server completion readiness. Unknown
completion is retained without an unbounded destructor join. The outer caller
reserves cleanup inside its original timeout, requests termination only of its
owned test process if needed, observes exit if possible, and reports unknown
otherwise. Files/partial records remain retained. No timeout certifies hard
real-time bounds on synchronous OS calls or scheduling.

## Primary contracts and alternatives

Reviewed official contracts 2026-11-10:

- [GetProcessHandleCount](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesshandlecount):
  returns executive-object handle count; nonzero success and native failure
  remain distinct. Count does not identify objects or measure retained memory.
- [GetProcessTimes](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes):
  creation time is two FILETIME words combined as the existing producer does.
- [OpenProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess)
  and [GetCurrentProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getcurrentprocess):
  access/noninherit flags and pseudo-handle ownership are explicit; querying the
  current process does not create another lifecycle owner.
- [CloseHandle](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-closehandle)
  and [closesocket](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-closesocket):
  process-handle close is not termination; sockets use closesocket. Successful
  socket close can precede outstanding/provider resource completion.
- [WSAPoll](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsapoll),
  [accept](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-accept),
  [recv](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-recv):
  finite readiness waits, actual nonblocking no-connection failure and actual EOF
  are separate observations.
- [WSASocketW](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsasocketw),
  [GetHandleInformation](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-gethandleinformation)
  and [OwnedSocket](https://doc.rust-lang.org/std/os/windows/io/struct.OwnedSocket.html):
  existing construction flags and raw ownership are reused; runtime flags are
  retained rather than guessed from a source comment.

Alternatives rejected: fake Control/process counters or copied connector states
cannot witness actual handles; public-only construction duplicates the existing
private producer; cross-process socket duplication/VM setup adds unrelated
authority; ProcessExplorer/NT handle inventories/install are unnecessary;
warm-up/count allowances conceal cold-state facts; sleeping or repeated probes
cannot establish the required causal close cut.

## Reproduction and caller recipe

`caller.ps1` runs on Windows without installing Python. It requires exact binary
SHA256 and independent source-matching `build-receipt.json`, one mode, new private
output path, repetition count and all budgets. It preserves separate native
stdout/stderr bytes plus process-exit/timeout receipt. PowerShell exit0 alone
does not pass the evidence gate or establish handle stability.

`caller.py` supplies strict pure parsing/validation and an optional Windows
launcher on machines with Python. It rejects zero-test/launcher-only output,
wrong modes, missing close0/EOF/join, malformed native widths, BOOL-as-integer,
failed count APIs and unsupported stability claims. For PowerShell artifacts,
feed original stdout+stderr bytes to `validate(mode, repetitions, exit_code,
output)` and retain the returned raw records/issues. The source and binary
hashes identify bytes; ROOT reviews their build association.

`pack-light.py` writes a small source archive, not a full worktree. Original
API/ABI/afunix/testdir bytes remain unchanged. NEW generated
`lib_handles_fixture.rs`/`client_handles_fixture.rs` add only module registration
in the private build context. Workspace/package metadata is narrowed and exact
original lock identities are retained. Native results from this actual component
are not a full original workspace/API, VM, nowait or seven-target gate.

The concrete assigned campaign uses three fresh processes, each with16 operations,
call1000ms, original case90000ms, cleanup5000ms and original outer120000ms.
These are explicitly selected fixture inputs and total resource/time bounds,
not production constants or performance allowances. No benchmark/profiling run.

## Actual Windows results and retained failures

Final check0, scoped test Clippy0 and source-matching `cargo test --lib --no-run`
build0 ran on Jupiter, Windows10.0.26200 amd64, rustc1.99.0
`b940084d7eb6a299eb4bfeb8e34901bc051e7ac4`, Cargo1.99.0 and LLVM23.1.1.
Private source/Cargo home/target only. All real dependency identities/checksums
remain those of the original lock; only the new private fixture package is added.
This is the actual API component with generated module registration, not a stub,
full workspace gate or later nowait source.

Frozen executable:1848320 bytes, SHA256
`d7e47c7f78c934b09be7e1e734dc8b134987a163059fc2703abfec803150ff4c`,
retained at `C:\Users\corph\native-borrowed-handles-3b-light-v1\artifacts\borrowed-handles.exe`.
Build receipt names the original API revision, full original/generated source
hashes and exact build arguments. Source snapshots and binary bytes were unchanged
after the final native campaign. No executable is committed.

| Final fresh native case | Actual OS/test outcome | Cold | Setup | All16 after-operation counts | After operations | Post join |
| --- | --- | ---: | ---: | --- | ---: | ---: |
| success | exit0; exactly1 passed,16 actual byte/drop/EOF/close0 receipts | 102 | 115 | 118 repeated16 | 118 | 112 |
| wrong-start | exit0; exactly1 passed,16 typed DifferentStart + native no-queued-dial receipts | 102 | 115 | 115 repeated16 | 115 | 109 |
| withheld | exit0; exactly1 passed,16 typed Read expiry + actual request/EOF/close0 receipts | 102 | 115 | 118 repeated16 | 118 | 112 |

All raw count calls succeeded. Native PID matches the caller's owned process
receipt. Listener flags were actually0; successful returned client streams and
accepted server connections reported flags0 where queried. The typed refusal
and every native flag/close/count record are retained, not reconstructed from
this table. Both success and withheld show higher post-join counts than cold;
the cause is **unattributed**. No count was subtracted, warmed away or called an
acceptable leak. These are functional outcomes and point observations only.

First native wrapper runs also reported one selected test passing per mode, but
PowerShell returned null ExitCode after HasExited. Those wrappers are retained
as **unverified**, never relabelled exit0. A narrow caller correction cached its
own process handle before waiting; the unchanged binary was rerun in NEW
`native-v2/` directories and returned actual numeric0 for all three modes.

Earlier retained fixture/transport failures: a readonly tool wrapper let the
outer shell expand a PowerShell variable; Python's WindowsApps alias was absent
(no installation attempted); append transport produced64000 extra base64
characters and failed SHA before extraction; numbered idempotent chunk files
corrected it; GNU tar treated C: as a remote archive and stopped before compile,
then relative-path extraction succeeded. First fixture compile check0/Clippy101
reported helper stdout macros and needless generic borrows; final new-fixture
Write serialization corrected these. A per-step readiness event was also made
explicit before the fast wrong-start operation. None is a production defect or
a passing gate, and no failing production behavior was patched.

Original Windows archive `raw/windows-records.tar.gz`:38339 bytes, SHA256
`b4c3f67771143058444bbc86053dcc1275b6ee243ec80103162b59abd14bc8da`.
Verified47 unique safe regular members and all46 manifest-listed member hashes.
Originals are retained at
`/Users/adalundhe/g121-retained-archives/native-borrowed-handles-3b-v1-b4c3f677`;
`raw/windows-decoded/` contains explicitly labelled decoded LF review derivatives;
log trailing whitespace/final blank lines are normalized only in these copies.
UTF16/BOM/CRLF originals remain immutable in the archive. `raw/decoding.json`
maps every original/derivative hash. `raw/native-validation.json` strictly checks
all final native records, PID/exit association and lock identities; first-wrapper
unknown exits remain failures. Private directory logical file bytes124720390
at collection are storage bookkeeping, not peak/cleanup/kernel measurement.

## R01–R16 account and remaining work

| Rule | Scoped evidence and limit |
| --- | --- |
| R01 | Production unchanged; test-only assertions permitted. Native failures/unknown counts and cleanup are explicit; no universal panic-free or OOM claim. |
| R02 | Reviewed exact3b seam, official contracts and alternatives; ROOT approved before code. |
| R03 | No production literal. Protocol/ABI units and caller-supplied bounds are documented above. |
| R04 | Real typed API outcomes plus explicit step/release/receipt/close states; no fabricated lifecycle/network success. |
| R05 | Existing handshake/readiness/held owner; bounded cooperative events, no new production manager/cache/poll. |
| R06 | One fixed owned server, one outstanding step, focused typed records; no pool or generic fixture manager. |
| R07 | Comments identify ownership, unsafe-call preconditions, cold semantics, close uncertainty and generated context. |
| R08 | No comparative or4–5× claim; all benchmark work remains coordinator-owned. |
| R09 | New unverified obligation, no new production defect/fix inferred. Wrapper/fixture failures remain separately retained. |
| R10 | Repeatable source/caller/budgets, exact source/binary hashes, actual logs and failure history retained. Native findings are revision/scope-specific. |
| R11 | Assigned native Windows amd64 component only; Windows ARM VM paused, other target/native/hardware integration remains unverified here. macOS arm64 only. |
| R12 | Existing shared API/afunix/identity seam; only Windows evidence adapters, no new production OS mechanism or authority. |
| R13 | New docs/fixture only; unapplied narrow registration patch. |
| R14 | Point-count correctness observations only; no allocation/kernel/peak/latency/resource-performance inference. |
| R15 | Initial implementation/light role expanded by explicit currentR15 native assignment; no audit/profile/benchmark/main merge/children/goals. |
| R16 | New private admitted source/Cargo/target, no protected cache reuse/deletion or local build. All source/failures/evidence preserved. |

ROOT retains review, actual parent registration/full original API and consumer
gates, seven-target/cross-lane integration, current nowait zero/MAX gates,
native process-death/clone/interruption/backlog/OOM and meaningful mutations,
supported real VM/hardware coverage and all audits/profiling/benchmarks.
No managed-plan/G22 closure follows from this checkpoint.
