# G22 finite borrowed connector candidate

Current production-source checkpoint: signed
`53535ab61225905acfa98f12dabdabb75138c808`, branch `p/managed-borrowed`, isolated
worktree `managed-borrowed`, based on combined `cbe3d027`. Experimental; no main
merge or adoption claim. Unchanged-source Linux results are in [LINUX.md](LINUX.md):
GNU units48/0 but integration14/1; native musl48/0+15/0, both strict clippy0.
The one timing-only GNU assertion is now replaced by an ordinary causal held-writer
regression, with exact red/restored mutation and GNU49unit+14integration/clippy0
at its named sources: [HELD-OUTPUT.md](HELD-OUTPUT.md). That test-only change does
not rewrite the earlier clock red or cure the Mac owned OOM abort.
An earlier4CPU/8GiB/10GiB request was denied for CPU headroom; the ask was not
reduced or runner guard bypassed. Root subsequently
authorized native Mac light checks: independent passes and the full-library RED
are recorded in [MAC.md](MAC.md). No ready-main/whole-library panic-free claim.
Native Windowsamd64 focused light results on exact12233203 subsequently passed
without production changes: [WINDOWS.md](WINDOWS.md). Mac full-library RED remains.

Current policy receipt: implementer full canonical g121-main AGENTS/R01–R16/evidence,
CLAUDE/plan/handoff, `f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Earlier checks retain their historical canonical c921a0b1 / rules SHA256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`; full AGENTS,
R01–R16, evidence, CLAUDE, managed plan/G22 and handoff read. Root owns audits,
profiling/benchmarks, heavy E2E/chaos/sweeps, final combined gates and merges. This
lane performed scoped implementation and light unit/socket fault checks only.

## Result and state contract

`connect_borrowed(&Identity, port, patience)` returns the existing boxed Stream.
It holds the existing Unix `taken::Held`, or a Windows non-inheritable
query+synchronize OwnedHandle, before reading process creation identity. It checks
identity/liveness, performs the existing Firecracker CONNECT handshake, checks the
held process again, and returns the connection. The held process object is closed
on every return; it never becomes Guest, lifetime ownership, watcher, signal/stop,
VM export or VM copy. Caller-supplied Identity remains the existing trusted input;
a PID/start check is not a new authentication protocol for the muxer socket.

The sequence is `validate finite deadline → hold/check process → dial → write
request → bounded reply → check held process → box stream → return`. Each step's
failure drops acquired objects. Zero expires before process/dial I/O. An
unrepresentable absolute deadline returns DeadlineOverflow before dial. There is
no borrowed retry, unbounded/MAX mode, new timeout default, polling loop or cache.
Local synchronous OS calls and scheduler delay are not a hard real-time guarantee;
all readiness waits use the original absolute deadline, and late I/O cannot become
successful handshake completion. New errors retain typed phase/cause; presentation
does not determine retry or identity decisions.

The owned Guest adapter preserves the documented zero one-dial/guest-answer mode
and MAX no-clock mode. The shared handshake now refreshes each partial write/read
from one absolute deadline and checks completion against it. CONNECT request and
OK response buffers are stack arrays bounded by protocol words + decimal u32
width + newline, preserving the prior accepted prefix/whitespace/u32 grammar and
unconsumed payload bytes. Existing Stream clone, shared settings, nanosecond wait,
nonblocking I/O, half-close and borrowed-handle interfaces remain.

## Allocation decision and proof

The coordinator reviewed and approved the alternatives recorded in
[README](README.md#allocation-alternatives-before-production-redesign), then the
exact Triomphe0.1.17 dependency (`default-features=false`, only std enabled), one
private fallible shared Socket state, and fixed-Socket Global allocation helper.
Cargo.lock changes only the API dependency list and that one package/checksum.
No allocator-api2 dependency, custom refcount, reserve-then-infallible allocation,
guessed std Arc layout or workspace-wide Arc migration was introduced.

The two old Arc allocations become one `try_new` shared state containing the same
read/write timeout and nonblocking atomics. The private fixed-Socket helper uses
`Layout::new::<Socket>()`, calls Global alloc, checks null, initializes exactly
once, then transfers sole ownership to std Box for ordinary drop/deallocation.
[Rust's Box contract](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.from_raw)
explicitly supports this exact allocator/layout construction. No generic allocation
framework is added. Every failure returns ErrorKind::OutOfMemory or typed refusal
without custom io::Error boxing or an allocating fallback String.

Triomphe Clone's refcount-abort precondition is excluded by the private ownership
mapping, not probability: only Socket owns that Arc; each additional strong count
is created **after** an independent owned OS duplicate and a nonoverlapping
Socket box have been acquired. A failed box leaves the count unchanged and drops
the duplicate. A forgotten returned box retains its allocation/count. Moves do not
increment the count; no Arc/count/raw operation is exported. On all supported
64-bit targets Socket occupies at least two pointer widths. Even using the entire
usize-addressable byte space, `1 + usize::MAX / size_of::<Socket>()` is below
isize::MAX; the1 accounts for the original possibly unboxed Socket. The layout
test checks these assumptions. Unix descriptors additionally use the OS's signed
int descriptor range. No undocumented fixed Windows handle limit is assumed.

Linux identity reading reuses pinned Rust1.98.1's fallible
[fs::read_to_string](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/fs.rs)
and [read growth](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/io/mod.rs);
only path construction/error representation changes. There is no new proc parser
or buffering framework. Unix reuses held pidfd/kqueue machinery. Windows uses
[OpenProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess)
with query-limited+synchronize access, shared GetProcessTimes creation extraction,
zero-time held state checks and OwnedHandle close. The narrow afunix adapter
carries the absolute deadline through WSAPoll interruptions; local clone uses
stack WSAPROTOCOL_INFOW through the existing
[Winsock duplication contract](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsaduplicatesocketw).
External share_with/from_shared APIs and wire formats are preserved.

## Compatibility and limits

The approved `Refusal::Allocation { what: &'static str }` is a source-compatibility
change: downstream exhaustive matches must add an arm. It identifies a post-boot
connection resource failure; callers must not classify it as Boot or Unsupported.
All earlier variants and their outer Display formatting remain. Inventory:
`api/src/{lib,client,sim,conformance}.rs`, `api/tests/sim.rs`; shards boot tests use
shards-api but do not exhaustively match Refusal. Current workspace consumers
compile in the native API light corpus; external consumers have not been built.

Handshake free-text why details now render typed phase, ErrorKind and raw OS code,
instead of io::Error Display's potentially allocating OS error_string. The typed
source remains available. Invalid socket-path/timeout errors retain their kind,
with generic nonallocating io::Error representation. These are explicit diagnostic
text/payload changes, not invisible compatibility. No wire/tag/crypto change.
The simulator's newly reachable pair/outer-box OOM is mapped directly to Allocation
before enqueue; its preexisting pending-queue/accept/Guest lifecycle allocations
are not certified panic-free by this focused change. Root was informed of those
preexisting out-of-new-borrowed-path hazards; they remain central review debt.

## Native light results and identities

Assigned runner `8fadc1ec213332e16cb0d915a3fb913f`, private source
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-borrowed-c5f6175fcc`,
target `~/managed-borrowed-target`, raw evidence `~/managed-borrowed-evidence`.
Rust1.98.1 `48a229cea`, LLVM22.1.8 selected by repo rust-toolchain.toml,
Fedora Linux6.19.10 amd64. Home-directory default1.99.1 is not these builds'
compiler. Root clones/caches, frozen RPK artifacts and paused Windows ARM VM were
untouched. The subsequently authorized private Mac build is recorded separately.

| Source/job | Actual result |
| --- | --- |
| 09e1a482 / r171 | Three actual pre-fix reds: zero and overflow issue CONNECT and remain blocked;200ms per-read handshake accepts dribbled OK at501.503476ms. Raw logs in r171/ |
| 9a29652d / r176 | Preimplementation ordinary units33 pass/0fail/6ignored; strict API clippy0 |
| ac8c2834 / r184 | Ordinary units45 pass/0fail/7ignored; integration15 pass/0fail/2 measurement ignored; real measurement1 ignored; strict all-targets API clippy0 |
| 11913786 / r185 | Six independent local guard mutations each cargo101 for actual forbidden effect/allocator abort; each restored focused test0, restored library corpus0 and strict clippy0. No VM/profile sweep |
| 389e823e / r186 | Additional candidate regression actually reproduced: queued byte with1ns wait incorrectly returns WouldBlock before initial I/O |
| 53535ab6 production / c5c8e4c6 source | Mac borrowed12/0, Stream11/0 (including ready-I/O regression), sim15/0, scoped clippy0; full lib RED via owned-OOM64B SIGABRT |
| 12233203 / r190 | Native Windowsamd64 compile0, borrowed12/0/1 child ignored, Stream9/0, strict API+afunix all-targets clippy0. Two earlier launcher setup failures retained separately; no production change |
| eaedd0b0 / r192 | Fresh GNU/musl build0 and strict clippy0. GNU units48/0, integration14/1 on inherited-output refusal timing bound; musl units48/0+integration15/0. All earlier owed native raw records recovered without mutations rerun |

The r184 corpus includes exact binary payload with u32::MAX port, clone and
half-close; withheld and dribbled reply deadlines; malformed/closed reply typing;
stale process start/no dial; dead process before/during handshake; actual child
reply after borrowed stream drop; state/stream-box/clone all-after-N allocation
failure; legacy String-refusal OOM; owned Guest direct Allocation; existing owned
zero/MAX cases. One successful socket case asserts Linux FD/task return. This is
bounded correctness evidence, not leak freedom or repeated resource accounting.

The r185 independent effects were: actual CONNECT with zero; actual CONNECT on
overflow; actual CONNECT for stale identity; returned stream after actual process
death/reaping before OK; accepted late dribbled OK; actual allocator abort after
replacing fallible Arc construction. Mutations restore their own exact private
source bytes in finally, touch owned lib.rs for recompilation, retain patch/log/
SHA provenance and re-run the restored test. They do not mutate canonical/root
source. Zero/overflow fixtures independently accept/read the request, release the
withheld peer, and join the worker after observation. Allocation faults are TLS
unit-test instrumentation, not linked into production or used for timing claims.

The low-level ready-I/O regression was introduced by this candidate, not attributed
to the old code. [socket(7)](https://man7.org/linux/man-pages/man7/socket.7.html)
defines these timeouts for blocking, supporting preservation of the initial
nonblocking operation. The handshake still checks its distinct overall deadline
before/after I/O. The1ns test is the smallest representable nonzero Duration, not a
production timeout default. EINTR/suspension/Windows boundary behavior remains
additional central fault-matrix work.

The old implicit-target `shards_api-995bfe5a5b6dd15f` was last built for r186.
Latest r192 instead used explicit GNU/musl target directories and actual fresh
compiler-selected binaries listed in [LINUX.md](LINUX.md). Do not reuse the old
implicit-target artifact as a latest passing binary. Historical results and
hashes remain named for their own revisions.

## Root replay and owed work

With the assigned4CPU/8GiB/20GiB admission and private target, after sync:

```sh
export CARGO_TARGET_DIR="$HOME/managed-borrowed-target"
touch crates/api/src/{lib,borrowed,channel,client,sim,stream,taken}.rs
cargo test --locked -j4 --profile test-release --target x86_64-unknown-linux-gnu -p shards-api -p shards-afunix -- --test-threads=1 --nocapture
cargo clippy --locked -j4 --target x86_64-unknown-linux-gnu -p shards-api -p shards-afunix --all-targets -- -D warnings
```

GNU refusal-timing assertion remains RED; a causal controlled-writer fixture
proposal is separate next work. The six historical guard mutations were not
rerun; their original raw evidence is now retained in linux-native-history/.
Root review of the narrow source/diagnostic text changes, [D215 migration note](D215-MIGRATION.md),
seven-target check/lint and remaining macOSarm64/Windows behavior remain owed. The Mac full-lib
owned-OOM abort remains RED. Root subsequently confirmed the actual
OnceBox→ShardsGuest::connect stack in separate coordinator tracing; no warming/skip
is used to call it green. Native repeated
Windows success/zero/overflow/identity/error handle counts, live VM borrowed
attach/drop/lifetime, large/dribbled/interrupt/connect boundary cases and aggregate
resource behavior are not passed. Root owns those platform/heavy gates plus any
profiling/comparisons and final combined source validation. There is no4–5x,
allocation-free, hard-quota, leak-free or managed-runtime-completion claim.

R01: borrowed allocation/error paths passed scoped injection on Linux/Mac; Mac's
owned cold-OOM full-lib abort and GNU integration timing assertion remain RED; existing
unrelated allocation hazards explicitly retained. R02: primary alternatives,
coordinator decision, actual red/green/failure data linked. R03: protocol/u32
buffer bounds, two directions, OS rights/layout and milliseconds units derived;
no runtime timeout/cap/cache constant added. R04: explicit phase/typed failures
and acquire/drop sequence. R05: one shared existing seam, held identity authority,
readiness+deadline waits, no periodic probe/cache/watcher. R06/R07: focused private
modules, resource and unsafe/precondition comments; no speculative manager.
R08/R14: performance work remains root-owned and unclaimed. R09: pre-fix native
red and candidate regression red retained. R10: reproducible fixtures/raw records,
actual-effect mutation/removal and restored runs; latest source gaps explicit.
R11: Linuxamd64, Macarm64 and Windowsamd64 scoped native checks recorded, with
exact-source/red limits above; Windows/Linuxarm64 and real VM hosts remain
unverified here.
R12: shared handshake/stream + minimal Unix held reuse/Windows adapter, rootless,
one installed binary, no security bypass. R13: concrete API and real failure causes,
no generic allocation framework. R15: signed isolated checkpoints, no children,
heavy campaigns or main merge. R16: private regenerable target, no unrelated cache
cleanup, no lowered admission ask or local floor violation.
