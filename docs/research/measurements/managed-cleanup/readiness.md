# Cleanup fixture readiness and owned-child failure paths

This repairs a **host test fixture**, not guest cleanup. Production init and the
guest cleanup workload are unchanged. The 64-pass sweep is neither replaced nor
justified here. The earlier four actual-VM results in [README.md](README.md) retain
their original source identity; they are not passing results for this revision.

Unix supervision is implemented and exercised. The coordinator-approved Windows
adapter restores the all-host fixture source and passed the narrow source
type-check below; native Windows and real-VM proof remain owed. The earlier
Unix-only stage must not replace all-host coverage. This checkpoint needs root's
coverage review before integration/main readiness.
Root owns real VM, mutation/repeat/resource/platform gates; implementer work is
limited to the assigned source and light native cases. No main merge or child
agents, host root, security changes, process-group kill or cache was introduced.

## Current shared native child (portability checkpoint)

The native factory now requires an absolute existing `CLEANUP_CLIENT_CHILD`
artifact, compiled from [readiness-child.rs](readiness-child.rs) with pinned
rustc1.98.1. It invokes that exact artifact with `MODE stdin`; no PATH interpreter,
Microsoft Store alias, shell, constructor, custom test-main or per-OS child script
is involved. This is a private test artifact, never installed, carried or shipped
as part of Shards. The Client, readiness deadline, Windows adapter, guest workload
and product cleanup are unchanged. The original Python helper/red scripts/raw
logs remain historical evidence, including their signal-only controls.

The actual Windowsr188 exit49 was an API-lane **launcher setup failure before
clone/compiler/native tests**, not a cleanup Client or product failure. Its
unchanged classification and original evidence revision are copied in
[native-child-evidence](native-child-evidence/). This demonstrates the runner's
missing interpreter, while source inspection identifies the cleanup factory's
same unwanted dependency; no Windows cleanup failure is invented.

The shared Rust child preserves exact stderr PID identity, silent/early/partial/
normal bytes, early exit7, one stdin release byte `r`, invalid/EOF release exit8,
and literal LF bytes. I/O failures return an error/status. Input modes and fixed
bytes/exits are existing fixture protocol identifiers, not runtime resource caps.
The standard [Command path contract](https://doc.rust-lang.org/std/process/struct.Command.html#method.new),
[byte-oriented Write contract](https://doc.rust-lang.org/std/io/trait.Write.html)
and [ExitCode](https://doc.rust-lang.org/std/process/struct.ExitCode.html) support
this small std-only seam. Those current docs display1.99.1; actual compile/run is
pinned1.98.1, recorded separately. Microsoft's
[execution alias explanation](https://learn.microsoft.com/en-us/windows/dev-environment/python#why-does-running-pythonexe-open-the-microsoft-store)
explains why an alias with arguments can error. Installing Python/changing aliases,
OS-specific child scripts and custom runtime entry mechanisms were rejected by
scope. None was added.

On macOS arm64, the helper compiled and passed **6/6** light protocol controls:
early7; silent/partial/normal withheld release and exact terminal bytes; wrong-byte
and EOF release8. Each control owns and waits for exactly its child; no process
group or foreign process is signaled. Strict helper clippy initially rejected a
byte-character slice spelling, retained as red, then passed after its suggested
byte-string spelling. Final compile/strict clippy exit0, literal bytes and child
identities are retained in [native-child-evidence](native-child-evidence/).
This is **not** a rerun of the five Client tests, Windows/native pipe behavior,
mutation, VM cleanup or a resource/performance claim; those remain root-owned.
Std I/O/argument/library allocation and uninterruptible OS wait limits are not
certified by this fixture. No production panic/allocator claim is made.

The optional Unix Python driver now compiles the Rust child and records its
source/artifact SHA256, then passes its absolute path. `--helper-only` runs just
the six light controls, with no Cargo/Client/mutation/VM build. Its Python is
orchestration only; **the current native test command below needs no Python**.
The complete driver still includes coordinator-only mutation/restoration, so an
implementer must select `--helper-only`. Mac compile was under
`lockf /tmp/g121-mac.lock` after checking the30GiB floor; no full crate/C build.
Current binding receipt: canonical g121-main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`, role
implementer, allR01–R16. Root owns all audits/profiling/benchmarks/heavy real E2E/
chaos/stress/sweeps; no subagents/main merge or other lane/cache changes occurred.

## Historical identity and actual red

Base combined `cbe3d027`; only the eleven cleanup fixture/evidence paths from
`fa1b5d35654ac395e4ce24b8fee8ac59f4f0555e` were composed (`cb0843eb`). Guest source
matches fa1 byte-for-byte. Canonical policy `c921a0b12b06d495b354907162e991376edffd41`,
rules SHA256 `1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`;
role implementer, with current user coordinator-only heavy-work restriction.

`readiness-red.py` embeds the actual unchanged fa1 `Client`, `ready` and `finished`
source in a standalone Rust probe. It substitutes only the executable factory:
`readiness-child.py` blocks on a signal, exits early, emits a partial readiness
line, or follows normal readiness/release. There is no VM or mocked read. A Linux
pidfd/selector supervisor bounds observation and explicitly rescues/reaps only
the controlled child. The baseline source stays pinned after the repair.

```sh
python3 docs/research/measurements/managed-cleanup/readiness-red.py \
  --patience-ms 500 --evidence "$HOME/managed-cleanup-quality-evidence/pinned-client-red"
```

Actual native r33 and compiler-context-corrected r35 both returned the expected
red exit1. Silent and partial `rea` children did not return within the caller's
500ms bound; only killing their held child unblocked the original `read_line`.
Early exit7 produced EOF/panic immediately. All three paths left an actual
`Z (zombie)` child until the probe explicitly waited for that child. Normal
readiness/release returned `released\n`, exit0, with no rescue. Every controlled
probe and rescued child was reaped; this is not a general leak-freedom claim.

r33's preserved post-run `environment` called rustc from home and reported its
default1.99.1; that field does not identify r33's compiler. r35 captures compiler
and cwd **before** building: Fedora6.19.10/glibc2.43, repo-pinned Rust1.98.1
(`48a229cea`), LLVM22.1.8, Python3.14.3. Both exact probe/source/scripts and raw
results are retained; r35 is the authoritative compiler-context reproduction.

## Decision and state

Rust's [Child contract](https://doc.rust-lang.org/std/process/struct.Child.html)
does not kill or wait on drop; completed children still require reaping. Existing
`common::Vm` supplies the kill/wait RAII pattern and reader-channel precedent, but
its readiness methods periodically sample progress and its reader handles are
detached. Reusing it unchanged would retain the assigned debt. A blocking reader
thread plus kill-CLI/join is also insufficient when the daemon retains the
delegated stdout writer; the read can still wait on another owner.

The repair therefore removes the reader helper. Existing Unix
`platform::Poller` watches nonblocking stdout and held-child exit. It registers
before the one status check, including the existing macOS already-exited-process
case. `[Starting → Ready → Finished]` is explicit. Prefix comparison permits
partial readiness without extending the caller's single absolute deadline,
carried from construction through connection/setup and readiness.
Completion drains actual bytes to EOF, then awaits the exit event before reaping;
it does not manufacture the guest cleanup report or tear the guest down first.
Errors distinguish phase deadline, EOF, invalid readiness, I/O and invalid state.

The Linux exit watch is retired after the actual exit notification: retaining an
always-readable pidfd while some other writer holds stdout would otherwise turn
the next wait into a busy loop. Nonblocking reads drain available input; they are
not periodic state probes. `[poll readiness](https://man7.org/linux/man-pages/man2/poll.2.html)`
and [file status flags](https://man7.org/linux/man-pages/man2/fcntl.2.html) are the
underlying contracts. No timer tick, new process manager, global reaper or cache
is introduced. Duct's [documented global-drop polling](https://docs.rs/duct/1.1.2/duct/struct.Handle.html)
would add another ownership mechanism and periodic work, so it was not imported.

RAII owns the Child before fallible descriptor/watch setup. Error/drop ends and
waits for only that Child; dropping its own stdout descriptor requires no blocked
reader to be joined and does not kill another writer. As with existing test
supervision, SIGKILL/wait cannot promise a bound against an uninterruptible kernel
task. No such general guarantee or actual VM cleanup result is claimed.

## Historical Client proof and current owed root runs

Five controlled native tests cover silent output, early exit, partial readiness,
normal release/exact terminal bytes, and dropping a ready but unreleased client.
Caller patience is required via environment; it is a fixture input, not a runtime
allowance. A single post-drop waitpid observation requires ECHILD. A failing
mutation is rescued before assertion, so the test does not leave its own child.
The portable controls use an owned stdin byte for normal release. On Windows the
witness retains this exact child's process handle and observes its terminal state;
it does not call that observation Unix zombie reaping or general handle freedom.
Those historical runs used Python's binary stdout buffer. Current native runs
use the shared Rust byte writer above; their exact bytes still require actual
per-platform execution, and compilation is not a Windows behavior pass.

```sh
mkdir -p "$HOME/managed-cleanup-native-artifacts"
# On macOS, check >=30GiB free and prefix rustc with lockf /tmp/g121-mac.lock.
rustc +1.98.1 --edition=2024 -Dwarnings -Ccodegen-units=4 \
  docs/research/measurements/managed-cleanup/readiness-child.rs \
  -o "$HOME/managed-cleanup-native-artifacts/readiness-child"
SHARDS_HELPERS=skip CLEANUP_CLIENT_PATIENCE_MS=500 \
CLEANUP_CLIENT_CHILD="$HOME/managed-cleanup-native-artifacts/readiness-child" \
CARGO_TARGET_DIR="$HOME/managed-cleanup-quality-target" \
cargo +1.98.1 test --locked -j4 --profile test-release -p shards \
  --test managed_cleanup native:: -- --ignored --test-threads=1 --nocapture
```

The historical private target was a reflink of inactive endpoint target after an
empty open-FD check; the source was not modified. That historical assignment was
4CPU/8GiB/10GiB. The current portability checkpoint ran only the tiny Mac helper
compile/control above; it ran no local crate/C build. Root Windows replay uses
its unchanged4CPU/8GiB/20GiB admitted envelope and private target.

Final Client source `43671fad` (unchanged in checkpoint `3ecff4e4`), SHA256
`0b434f8ef2397046000ba61923d58cfdcc1a315074f4e16c2bdbce4cd2bff50b`:

| Source / run | Observed result |
| --- | --- |
| `23f5f897` / r37 | Unix native5/0. |
| `23f5f897` / r39 | Independent drop-wait removal0/1 failed with actual owned child needing rescue; restored5/0. Strict clippy101 found the old signal test's safety-comment placement. Preserved. |
| `8a29fa30` / r41–r42 | Portable stdin controls: native5/0, strict clippy0, actual Windows Client/adapter/native source type-check0. Independent drop-wait removal101; restored5/0 and clippy0. |
| `43671fad` / r43 | Final one-deadline source: green5/0 → independent drop-wait mutation0/1 → byte-identical forced rebuild/restored5/0; strict clippy0; actual Windows source type-check0. Wrapper exit0. |
| `3ecff4e4` / r45 | Same Client with explicitly binary child protocol: green5/0 → independent drop-wait mutation0/1 → byte-identical forced rebuild/restored5/0; strict clippy0; actual Windows source type-check0. Child source hash also recorded. |

The final mutation's failing waitpid snapshot was0: the test had to finish/reap
its owned child after the removed wait. That is an ownership effect, not merely
a missing log. The guarded version required ECHILD. The driver rescues before
assertion and restores the source in `finally`; source bytes/hash were verified.
Passing wrapper exit0 includes the deliberately red mutation and is not a
passing real-VM/workspace suite. Actual source rebuilds were forced after sync
and restoration to avoid the previously observed Cargo mtime artifact trap.

Repeat the complete light proof with an unused evidence path:

```sh
CARGO_TARGET_DIR="$HOME/managed-cleanup-quality-target" \
python3 docs/research/measurements/managed-cleanup/run-readiness.py \
  --patience-ms 500 --source-ref "$(git rev-parse HEAD)" \
  --evidence "$HOME/managed-cleanup-quality-evidence/shared-native-child-proof"
```

[`readiness-evidence/`](readiness-evidence/) retains baseline and fixed commands,
source/hash/compiler identities, raw logs, deliberate failures and restoration.
The Windows check extracts the actual Client/adapter/native test source, pins
root's windows-sys0.61.2 and uses real existing IPC/ABI code with the installed
amd64 MSVC Rust target. It type-checks that seam without requiring the unrelated
full binary's C/SDK build. Full Windows crate/lint/native/WHP checks remain unrun;
no Windows behavior or connect-race success is inferred from compilation.
Root can exercise the same five native cases on Windows using its own positive
patience and private target (PowerShell example; not run here):

```powershell
$artifactDir = Join-Path $env:USERPROFILE 'managed-cleanup-native-artifacts'
New-Item -ItemType Directory -Force $artifactDir | Out-Null
$childSource = (Resolve-Path 'docs/research/measurements/managed-cleanup/readiness-child.rs').Path
$env:CLEANUP_CLIENT_CHILD = Join-Path $artifactDir 'readiness-child.exe'
rustc +1.98.1 --edition=2024 -Dwarnings -Ccodegen-units=4 $childSource -o $env:CLEANUP_CLIENT_CHILD
if ($LASTEXITCODE -ne 0) { throw 'shared native child compile failed' }
Get-FileHash -Algorithm SHA256 $childSource
Get-FileHash -Algorithm SHA256 $env:CLEANUP_CLIENT_CHILD
$env:SHARDS_HELPERS = 'skip'
$env:CLEANUP_CLIENT_PATIENCE_MS = '500' # explicit synthetic test input
$env:CARGO_TARGET_DIR = Join-Path $env:USERPROFILE 'managed-cleanup-native-target'
cargo +1.98.1 test --locked -j4 --profile test-release -p shards --test managed_cleanup native:: -- --ignored --test-threads=1 --nocapture
if ($LASTEXITCODE -ne 0) { throw 'native cleanup Client controls failed' }
```

After coverage review, root's unchanged real-VM contract invocation is:

```sh
env -u SHARDS_HELPERS CARGO_TARGET_DIR="$HOME/managed-cleanup-vm-target" \
timeout 600 cargo test --locked -j4 --profile test-release -p shards \
  --test managed_cleanup \
  main_cleanup_reaps_owned_children_and_preserves_independent_joiner \
  -- --ignored --exact --test-threads=1 --nocapture
```

Use a private4CPU/8GiB/20GiB VM campaign with actual carried helpers. Existing
guest counts8/72, 90s outer patience and guest observer deadlines remain synthetic
inputs from fa1, not evidence for64 or new production numbers. VM/mutation repeat,
daemon/guest/kernel resource return, faults/copies/allocations, neighbors and all
other platform/libc/architecture results remain coordinator-owned and unverified.

## Windows adapter and ownership

The public `shards_ipc::Listener::new` already creates a first-instance, owner-only,
remote-refusing overlapped server. `Listener::accept` returns a plain-byte Stream.
The approved test-only adapter reuses that listener, independently opens its
client with `CreateFileW(GENERIC_WRITE, OPEN_EXISTING)` **without**
`FILE_FLAG_OVERLAPPED`, gives that synchronous handle to child stdout, then accepts
and drops the listener. Connect completion waits on the existing listener event
and caller deadline **before one accept**, with no retry. Parent reads and process exit use the existing
overlapped `Stream`/`ipc::Poller`; no reader thread, new runtime API, repeated
PeekNamedPipe or CancelSynchronousIo race is needed.

The per-handle modes are explicit in Microsoft's
[Named Pipe Open Modes](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-open-modes)
and [Named Pipe Client](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-client).
An overlapped handle cannot simply be used as synchronous std stdout:
[WriteFile](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-writefile)
requires its OVERLAPPED argument in that mode. The command's retained stdout
handle is also released after the spawn result, including failure, so it cannot
keep its own reader alive. Names use the held process ID and a checked monotonic
counter; a first-instance collision or exhaustion returns an error instead of
adopting another endpoint or looping over names. The finite Win32 millisecond
range is checked and the INFINITE sentinel is refused. No deadline becomes an
unbounded wait through conversion.

The adapter is implemented and narrowly type-checked, **not natively exercised**.
Native Windows ordinary
child/readiness/byte/handle-return proof and WHP remain owed; paused Windows ARM
stays untouched. Root approved this necessary adapter before implementation and
owns its native connect-order/byte/handle-return and real-VM proof.

## Current helper R01–R16 account

R01/R04/R06/R07/R13: std-only typed modes, handled I/O and existing exit/byte
protocol; only native launcher changed, no Client/guest/product edit. Tests may
assert as before. R02/R03/R05/R12: primary contracts, real missing-interpreter
setup evidence and rejected alternatives above; same shared source on all hosts,
stdin completion event, no timers/cache/manager or new product constant.
R08/R14: no measurement, allocation-free/leak-free or4–5x claim. R09/R10: launcher
red classified exactly, old evidence preserved, actual6/6 helper controls and
clippy red/green retained; new Client/Windows/VM/mutation results unrun.
R11: macOS arm64 helper only; current native Client tests, Windows amd64/arm64,
Linux gnu/musl, full targets and real VM coverage remain owed to root.
R15/R16: current policy receipt above, scoped light work only, signed named-path
checkpoint, no subagents/main merge/rootcache; Mac floor/lock respected and all
source/raw evidence preserved. No local workspace/C build or cleanup.

## Historical R01–R16

| Rule | Evidence and remaining limit |
| --- | --- |
| R01 | Production unchanged; fixture RAII owns before fallible setup. Final Linux native5/0/strict clippy0 and actual Windows seam check0; OS wait/native VM limits above. |
| R02 | Actual raw red, Rust contract, existing supervisor/poller and Windows primary alternatives above. No product architecture adopted. |
| R03 | Caller supplies patience; readiness bytes are the existing exact protocol literal. No new production bound;64 remains unjustified. |
| R04 | Starting/Ready/Finished plus typed phase/EOF/error outcomes; no text-based control inference. |
| R05 | Removed blocking reader abstraction; actual descriptor/exit events and one deadline. No cache or periodic probe; Windows native proof held. |
| R06 | One private Client serves actual VM CLI and controlled native cases; no generic manager. |
| R07 | Comments document registration, exit-watch retirement, owned-child signaling and explicit staged coverage. |
| R08 | No benchmark/ratio or4–5× claim. |
| R09 | r33/r35 actual missing/partial readiness and unreaped child before repair; not a product cleanup reproduction. |
| R10 | Baseline source/probe/raw results, final green, actual independent reap-guard mutation and restoration retained. Root VM/deadline/control mutations still owed. |
| R11 | Linux amd64 native5/0; Windows amd64 actual seam type-check only. macOS/native Windows, full other targets/libcs and actual VM proof unverified; coverage review required. |
| R12 | Existing Unix platform and approved IPC Windows listener seam. Necessary private test adapter only; no installed helper or host root/crypto change. |
| R13 | Fixture-only repair, no guest cleanup or64-pass replacement. |
| R14 | Only owned-child reaping observation planned here. All broader resource/performance claims remain unmeasured/root-owned. |
| R15 | Scoped implementation/light native tests only, signed named commits; root owns heavy proof/integration. No nested agents/main merge. |
| R16 | Private clone/target/evidence, unchanged admission guard, preserved reflink source and all raw failures; Mac floor respected. |
