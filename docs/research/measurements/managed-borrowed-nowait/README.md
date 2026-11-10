# Borrowed readiness-only compatibility checkpoint

Feature adjustment on signed `3b3817eea489d107fb9820f0f46c4fe429709fa3`, using
the same public API and shared handshake. This is not adoption of an old vendor
branch or completion of G22/P3/P8. Full current native/Windows/OOM/handle/VM,
mutation, resource, profiling and matrix gates belong to ROOT and remain owed.
The separate owned-Guest macOS Mutex OOM defect remains open.

POLICY_READ implementer `/root/observer_reconnect`; coordinator `/root`. Full
canonical AGENTS.md, rules R01–R16, evidence, CLAUDE.md, current managed plan and
updated `/Users/adalundhe/g121-handoff.md` read before edits. Canonical main then
f9030566bc227d50770bba879bd1387ebffe410f; ROOT subsequently reported main/combined
24687c98f8c507981d91eb887d511fa7f4e2b83a (test/evidence changes). Policy unchanged:
last rule revision 664541da5ec488446b481ff6509b60510912c231; SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.

Existing worktree managed-output-exit reused on NEW p/native-borrowed-nowait-v1
from 3b. c70558/79f85d refs and artifacts remain preserved. Owned source is only
API client/lib/stream, the necessary afunix readiness adapter, and this new
measurement directory. Cargo manifests/lock/dependencies, taken/process parser,
ABI/kernel/VMM, native-admission fixtures, old focused evidence and owning Guest
state remain unchanged. Owning Guest implementation and shared Socket waits
module are byte-identical to 3b; actual owning zero/MAX tests were not run here.

## D-NW-1: concrete caller and wait modes

Actual caller: `connect_borrowed(&Identity, u32, Duration) ->
Result<Box<dyn Stream>, Refusal>`. ROOT approved this compatibility followup before
code after inspecting current sources and official alternatives. The existing
owned Guest adapter and borrowed caller now select explicit modes in the existing
single CONNECT core:

| Mode | Caller and state transitions |
| --- | --- |
| Ready | Borrowed zero: hold/check identity, one nonblocking connect, progress-bounded request/reply I/O, held recheck, fallible box, return. Any pending/interrupted operation returns its typed phase/cause and drops the owned socket. |
| Until(Instant) | Borrowed positive: create the absolute deadline before identity work; preserve it through setup, each partial operation, held recheck and boxing. Late completion refuses. |
| Unbounded | Existing owning adapter only; its prior zero one-dial/await-answer and MAX no-clock/retry behavior stays unchanged. |

Borrowed MAX or an unrepresentable deadline returns existing
`Refusal::Bounds { what: "patience", why }` before identity/dial. The existing
fallible message formatter builds `why`; allocation failure returns existing
`Refusal::Allocation` with static context and no allocating fallback. The earlier
BorrowFailure::DeadlineOverflow variant remains available for existing exhaustive
consumers but is no longer the borrowed patience result. Other typed borrowed,
process and allocation causes remain unchanged.

Ready never aliases a timeout of None to mean zero. Its connect branch is explicit;
read/write use one underlying nonblocking operation through Socket::read_nowait /
write_nowait, bypassing the normal readiness/EINTR loop. The unchanged normal
Read/Write implementations share those raw operations inside their existing wait
wrapper. Ready does not retry Interrupted. Partial successes strictly reduce the
remaining fixed CONNECT request or advance the bounded OK reply; pending/failed
progress ends the attempt. Normal returned Stream timeout settings are cleared at
the shared successful exit, and no normal shared nonblocking flag is changed.
A successful zero handshake requires every required operation to be immediately
available; it is not a guarantee that a listening peer can answer in time.

On Windows, the existing afunix connect/setup/SO_ERROR implementation has explicit
Ready/Until/Unbounded modes. Ready makes one connect and, when pending, at most
one WSAPoll with zero timeout, checks SO_ERROR when readiness is reported, and
returns WouldBlock when still pending. Interrupted returns immediately. No
connection retry, blocking poll or new timer/manager is added. This Windows branch
has source review only here; its compilation/native behavior remains ROOT-owed.

The supplied identity/socket remains trusted input; a held PID/start does not
authenticate the socket peer. No Guest, watcher or lifetime authority is acquired.
A failed attempt closes its socket; it cannot undo already-written CONNECT bytes.
Synchronous OS identity/setup calls and scheduling are not hard real-time bounded.
No cache, periodic probe, new poll loop, arbitrary timeout/cap or production default
is introduced. Existing zero/negative poll sentinels come from OS contracts;
request/reply limits and progress bounds come from the existing u32 protocol.

Primary sources and alternatives inspected:

- [Rust UnixStream](https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html#method.set_nonblocking):
  OS nonblocking mode and SO_ERROR; zero socket timeout is InvalidInput while None
  permits indefinite blocking. Online rendering was 1.99.1; actual checks used 1.98.1.
- [Microsoft connect](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-connect):
  WSAEWOULDBLOCK denotes pending connect; repeated connect calls are not a completion test.
- [Microsoft WSAPoll](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsapoll):
  zero returns immediately; negative waits indefinitely; error readiness is also reported.
- [Microsoft getsockopt](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-getsockopt):
  SO_ERROR retrieves/clears error status. Existing afunix take_error is reused.
- Exact current 3b client/Socket/afunix sources and its pinned borrowed evidence
  establish the actual two callers, existing held rechecks and fallible storage.

Alternatives rejected: mapping zero/MAX to None reintroduces an indefinite wait;
Some(Duration::ZERO) is not a readiness operation; changing normal Socket EINTR
behavior would silently affect other callers. A second handshake, generic wait
manager or test-only callback transport is unnecessary. Focused tests move to this
new harness; old managed-native-borrowed/focused.rs is retained unchanged as
historical evidence, including its intentionally superseded zero/overflow claims.

## Actual light evidence and retained failures

Kiln admitted b24c5c951260dcdfd0972f2aeaf3cf51 at 1 CPU/1 GiB/2 GiB. New private
source/CARGO_HOME/targets under `/Users/adalundhe/native-borrowed-nowait-20261110-v1`;
all protected ROOT clones/caches stayed untouched. Actual macOS arm64 Darwin 25.2.0,
rustc 1.98.1 `48a229ceaefd4985c50990b14116b6d856af0985`, LLVM 22.1.8. Build jobs and
codegen units were one. Actual private storage was 209,956 KiB; free space before
95,071,752,192 B and after 94,775,848,960 B. This is storage bookkeeping, not a
resource benchmark or measured hard quota proof.

The small actual API/ABI/afunix/testdir workspace preserves all 35 source/manifest
hashes. Its only derivative metadata is the smaller workspace manifest and exact
original 17-package lock closure; registry versions, sources and checksums were
not regenerated. Repository Cargo files are unchanged. This is not a full
workspace/matrix gate. [Source association](raw/source-association.json) verifies
all packaged hashes; baseline production is 3b with only its test-module path
changed to compile the same new contract examples.

Final v3 commands used separate NEW target-before-v3/target-after-v3 directories,
private CARGO_HOME and RUSTUP_TOOLCHAIN=1.98.1, `--locked -j1 --profile test-release`:

- Baseline two new contract examples: exit101, 0 passed/2 failed, actual earlier
  Borrowed Expired and DeadlineOverflow. These are feature-contract reds, not
  claims that 3b violated its prior documented behavior.
- Current two identical examples: exit0, 2 passed/0 failed. Zero's valid current
  identity/listener actually receives exact CONNECT before accept, then its peer
  independently reads EOF; a second accept is WouldBlock. Overflow is Bounds with
  invalid identity and no accepted connection.
- Six retained focused regressions plus the new raw-operation check: 7 passed/0
  failed. Identity mismatch covers zero and positive; exact bytes/clone/half-close,
  withheld/dribbled total deadline, malformed replies and refcount bound remain.
- Existing queued-I/O check: 1 passed/0 failed. Scoped actual library clippy: exit0.

Cargo reported fresh=false for both selected test artifacts, frozen separately:
baseline 214db503970cee76336d5957b910b4976bb0dc45c25c8cccb78e704909c683b9
(1,603,496 B), current 99556910993e9aa1d863ee4cfdfe79f20c0ca57e06db80787a24c7f9a46c7cc2
(1,649,896 B). [Results](raw/results-v3.json), exact submitted drivers and all raw
logs/manifests are retained in [the verified 42-member archive](raw/light-records.tar.gz).
No executable is committed. [Transfer identity](raw/transfer.json) names the
archive SHA and exclusive local extraction; every regular member hash was checked.

Earlier results remain retained and distinct: two Linux headroom rejections;
276's disk preflight rejected at 5.92 GB before build; first local packaging
stopped at unavailable tomllib before creating sources. Driver v1 compiled the
baseline then reused it for after (fresh=true, same binary): that after result is
INVALID. Separate fresh v2 targets produced distinct binaries: overflow passed,
zero correctly reached Read/WouldBlock, then the observation fixture's accepted
peer.set_read_timeout(Some200ms) returned macOS EINVAL22. That was 1 pass/1 fixture
failure, not a product diagnosis. ROOT approved only making that peer nonblocking,
progress/expected-request-length-bounded reads without retry on WouldBlock or
Interrupted, exact CONNECT, independent EOF and second accept. v3 passes this
stronger observation; product hashes stayed unchanged throughout. No reason for
the timeout setter's EINVAL is inferred, and no library timeout workaround,
warmup, skip or production fix followed it.

[prepare-light.py](prepare-light.py) reproduces the small source packages;
[run-light.py](run-light.py) is exact v3 execution text; driver-v1.py and driver-v2.py
retain the earlier submitted programs. Original source packages, caches, raw data
and frozen binaries remain preserved. Linux GNU/musl, actual Windows adapter,
ready-zero successful public handshake, EINTR/fault tracing, OOM fallback injection,
held-death/drop, handle return, full API/owned/cold-Mutex, VM and matrix gates are
unrun here and owed to ROOT. No old source gate substitutes for them.

## R01–R16 handoff

| Rule | Scoped evidence and remaining work |
| --- | --- |
| R01 | No production assertions/unwraps added; checked request bounds, typed phase causes and existing fallible diagnostics/storage. Strict lib clippy0. OOM injection/whole-production proof remain ROOT-owed; owned Mac Mutex unchanged/open. |
| R02 | Actual callers, official primary sources, rejected alternatives and ROOT-approved concrete mode seam above. No broader architecture selected. |
| R03 | Zero/MAX are requested API semantics; poll0/-1 and protocol/u32 progress bounds are established contracts. No new production timeout/default/cap. |
| R04 | Explicit Ready/Until/Unbounded and hold/connect/exchange/recheck/return; no sentinel alias or inferred lifecycle owner. Refusal remains typed. |
| R05 | One existing handshake and real raw-operation callers; normal waits unchanged; no cache/polling probe/retry manager. |
| R06 | Narrow shared branch selection and existing bounded exchange; source and focused checks retained, coordinator review owed. |
| R07 | Comments explain one-call interruption, compatibility, protocol and ownership obligations. |
| R08 | No comparative/4–5× result; baselines and targets remain ROOT work. |
| R09 | Explicit feature adjustment; identical new live contract reds/greens retained. Fixture EINVAL and invalid cache result preserved without guessed production fix. |
| R10 | Exact commands/source/binary association/raw logs and failures retained. Real guard-removal effects and native failure/cleanup remain owed. |
| R11 | Only named macOS arm64 focused results count; all other platforms/Windows adapter and VM combinations unverified. Paused Windows ARM untouched. |
| R12 | Existing client/Socket/afunix seams; no new dependency, kernel/VMM, root/admin, installed binary, trust or crypto change. |
| R13 | Same public API, no duplicate implementation or generic manager; owning behavior preserved. |
| R14 | No performance/allocation-free/leak-free claim; ROOT owns all profiling/benchmark/fault/handle campaigns. |
| R15 | Scoped implementer, no children/goals/main merge; signed named checkpoint and owed source/matrix/actual runs returned. |
| R16 | Existing worktree and new private small packages only; floor/headroom rejections honored, no root cache or user/evidence/artifact deletion. |
