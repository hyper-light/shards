# Current-main borrowed connector checkpoint

G22 / P3 / P8 implementation; coordinator review and current-source native gates
remain owed. This is a small adaptation on `eb5654d6f5356cacaca536ff51f14a632fa7e801`,
not a merge of the historical borrowed branch. It does not close G22, the managed
plan, the platform matrix, or the separate open MR-OOM-MAC-1 owned Guest Mutex abort.

`POLICY_READ` implementer: source/origin-main
`eb5654d6f5356cacaca536ff51f14a632fa7e801`, development-rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Full AGENTS, R01–R16, development-evidence, CLAUDE, managed-runtime-plan and
`/Users/adalundhe/g121-handoff.md` read. Root owns all audits, profiling,
benchmarking, heavy E2E/chaos/sweeps, combined validation and main merges.
Implementer owns scoped source plus pure/fake-muxer light checks only. No children.

## D-NB-1: retain the reviewed finite contract at the existing API seam

Actual Hex caller: `connect_borrowed(&Identity, u32, Duration) ->
Result<Box<dyn Stream>, Refusal>`. The historical finite production candidate is
`53535ab61225905acfa98f12dabdabb75138c808`; later focused source is
`122332036eb689c33fd52b4a065c53c7dc5e4530`. Its retained decision/raw records are
at branch-tip `f7604be619e18455b6ccf1c20e13945f1a315809` under
`docs/research/measurements/managed-borrowed/` (`CANDIDATE.md`, `README.md`,
`LINUX.md`, `MAC.md`, `WINDOWS.md`, `HELD-OUTPUT.md`). Inspect them reproducibly
with `git show <full-revision>:<path>`; these old results are not passes of this
adaptation. Current main's newer typed process failures and fallible raw-byte
Linux starttime parser are preserved.

The sequence is explicit: finite-deadline validation, process hold/start/liveness
validation, one connect, request write, bounded reply, held-process recheck,
fallible stream box, final deadline check, return. Every failure drops acquired
objects. Zero returns typed `Expired` before process/dial I/O; an unrepresentable
absolute `Instant` returns typed `DeadlineOverflow` before dial. Each readiness
wait and partial handshake I/O uses the original deadline; late completion is
refused. Synchronous OS calls and scheduling are not hard real-time bounded.
There is no borrowed retry, unbounded/MAX mode, cache, periodic process probe,
watcher, thread, Guest/adoption, VM stop/copy/export, or lifecycle authority.
The held process handle is closed on return, including successful return; only
the stream escapes. A PID/start match is not socket-peer authentication; the
existing trusted identity/socket input contract is unchanged.

The common CONNECT core is adapted from the historical channel implementation
inside existing `client.rs`. Owned Guest keeps its documented zero one-dial and
MAX no-clock behavior through its existing adapter; positive finite handshakes
now use a total deadline instead of renewing the budget per byte. The existing
owned retry pacing is retained, not certified or benchmarked here. No other
wait/cache is introduced. Handshake request/reply arrays derive solely from
`CONNECT ` / `OK `, decimal `u32::MAX` width and newline. The `1` in byte reads is
one protocol byte; zero denotes no progress/EOF. Timeout conversion retains
poll/WSAPoll milliseconds and signed input maximum. No production default or
measured number is added.

The candidate's separate BorrowError is adapted to `Refusal::Borrowed` with
`BorrowFailure`; process observations retain existing `Refusal::Process` PID,
expected start and typed cause. Handshake cause snapshots carry phase,
`ErrorKind` and native code without allocating diagnostic text, following main's
ProcessFailure representation. No-code causes remain explicit. `Refusal`
continues to implement Clone/Eq; exhaustive downstream matches must add the new
variants, including Allocation for owned connection storage/diagnostic failures.
Exhaustive ProcessOperation/ProcessFailure consumers also need the new Windows
operation and unexpected-wait variants. Windows process operations and unexpected wait result remain typed.
The owned handshake's diagnostic wording changes to these typed phase/cause
renderings. Its existing String adapter now uses the reviewed fallible formatter
and reports Allocation without a fallback allocation; successful owned streams
also use the reviewed fallible box. Borrowed errors never use this String boundary.
Existing owned-state locks and other owned/simulator allocations are not certified
panic-free by this scoped adaptation.

Root concretely reviewed and approved the exact current-main-to-candidate
stream/afunix/dependency diff and expanded ownership before those files changed.
Only the Triomphe package/API edge is added to the current lock; hyper-quic and
all unrelated package entries remain unchanged. Some-duration adapters now
refuse deadline overflow as nonallocating InvalidInput rather than flattening
checked_add to None; stream timeout overflow likewise cannot select no-clock.
Windows checks the deadline around setup/success as well as interrupted WSAPoll.
Owned zero/MAX semantics remain explicit at the caller. The previously
reviewed candidate compares stable std/nightly allocator API, fallible_collections,
fallacy-arc, allocator-api2, bespoke refcounts and pinned Triomphe. Its selected
construction is one private fallible Socket shared state plus exact-Socket Global
storage, with no bespoke refcount or guessed standard Arc layout. Primary sources
revisited for this adaptation:

- [Triomphe 0.1.17 Arc source](https://raw.githubusercontent.com/Manishearth/triomphe/v0.1.17/src/arc.rs):
  checked layout/null-return `try_new`; Clone's refcount-abort precondition requires
  the candidate's private owned-socket/nonoverlapping-box proof, not a guessed
  handle limit.
- [Rust Box allocation contract](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.from_raw):
  exact Global allocation/layout initialization and sole ownership transfer.
- [Microsoft WSAPoll](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsapoll):
  positive milliseconds, zero immediate, negative indefinite; a borrowed finite
  deadline must never map overflow to the indefinite sentinel.
- The historical candidate records the existing Unix held pidfd/kqueue reuse,
  Windows OpenProcess query+synchronize/non-inheritable rights, GetProcessTimes,
  and Winsock stack protocol-info clone sources and alternatives.

## Reproduction and evidence limits

Historical native red source `09e1a482e7f333e9abb4e88bafbe7cdbda6a7155` / r171
actually observed CONNECT for zero and overflow and accepted a dribbled reply
past a positive total budget. Their source/reference SHA and raw logs remain
retained at the full historical branch tip above. The later six independent
mutations, restored runs and focused native Mac/Windows results are likewise
revision-specific historical evidence. They have not been repeated for this
current-main adaptation. No old branch whole-merge, historical binary reuse,
whole-library panic-free, allocation-free, leak-free, performance or 4–5× claim.

`focused.rs` adapts only pure/current-process and fake-muxer tests: guard typing
before I/O, identity mismatch before dial, exact payload/u32-max/clone/half-close,
withheld and dribbled replies, malformed/closed responses, and source for a Linux test-only
allocation-failure check (explicitly coordinator-only and not run here). Its fixture inputs reuse `UNSERVED_PATIENCE`; fixture release
multipliers and byte delays are test inputs, never production policy. The
historical actual child-death/drop, repeated handles, native and VM campaigns
remain owed to root. Existing Linux test-only allocator is reused unchanged.

Current commands/results and exact hashes are recorded below. A source/mechanical result is not live reproduction
of the current full public caller. No warm-up or skipped OOM replaces a failing
full-library test. Main's separate owned-Mac Mutex defect remains open.

## Actual current-source light results

Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, Linux
`6.19.10-300.fc44.x86_64`, Rust1.98.1 `48a229ceaefd4985c50990b14116b6d856af0985`,
LLVM22.1.8. Admission requested and accepted1CPU/1GiB/2GiB. New private source,
Cargo home and target are under `/home/adalundhe/native-borrowed-eb565-v1/`;
no root-owned source/target/cache was used. Final source is `source-v2`.
The entire private directory, including frozen executable, was99,644KiB when
recorded, below the requested disk envelope. This is storage bookkeeping, not a
resource/latency/allocation benchmark or hard quota proof.

The light workspace retains unchanged actual API/ABI/afunix/testdir source,
workspace metadata/dependencies/lints/profiles and this focused harness, while
omitting unrelated workspace members and vendor patches. It is not an exact full
workspace gate. Its lock is the17-package transitive closure of exact original
lock records: versions, registry sources and checksums are unchanged. Full original,
light derivative and their comparison are retained under `raw/`. All31 source
and manifest hashes match locally: `source-identity-check.json` and
`final-source-hashes.json`. Only the deliberately reduced light lock differs.
`light-workspace-Cargo.toml` is the exact test workspace manifest.

With private `CARGO_HOME`, `CARGO_TARGET_DIR` and `RUSTUP_TOOLCHAIN=1.98.1`:

```sh
cargo test --locked -j1 --profile test-release -p shards-api --lib \
  client::borrowed_tests -- --test-threads=1 --nocapture
cargo test --locked -j1 --profile test-release -p shards-api --lib \
  stream::tests::already_queued_data_is_read_before_waiting_out_a_timeout \
  -- --exact --test-threads=1 --nocapture
cargo clippy --locked -j1 -p shards-api --lib -- -D warnings
```

Implementer light run r10, final-source results: focused7 passed/0 failed/1 ignored;
ready-I/O1 passed/0 failed; scoped library clippy exit0. The ignored new allocation
fixture is root-owned and unverified. No full library/simulator suite, real child
liveness/death/drop, fault campaign, Windows/macOS, VM or matrix test ran here.
The earlier source-v1 focused7/0 result is retained in `raw/focused-v3.*`; it is not
substituted for final source-v2 results in `raw/final-focused.*`, `ready-io.*` and
`scoped-clippy.*`. The new source adds no test warming or skip to the existing
owned-Mac OOM test.

Final frozen unit binary:
`/home/adalundhe/native-borrowed-eb565-v1/artifacts/final-api-unit`,
10,636,416B, SHA256
`9d8d5bb1ab3daae209a2970f6f59ad10641f17e3cb322750783a122f97a9afc5`.
It includes the existing test-only Linux allocator; it is not a production binary.
Raw archive18,282B SHA256
`b8ebb0a5a217d371936f1456755faf1976a2e65197523eb5ecba0e069db0aaf3`,
verified safe regular text members/no executable magic before local extraction.
Source-v1 archive and source-v2 delta identities are retained with both archives;
no historical other-agent artifact was used or overwritten.

Initial harness failures are retained, not product/test results: the first source
transfer hit the OS argument-size limit before process execution, then succeeded
with16,000-character base64 chunks; the full workspace lock refused the deliberately
smaller workspace under --locked before compilation (`raw/focused.*`); generating
a fresh light lock would select libc0.2.191 and sha20.11.1, so its identity assertion
stopped before compilation and those generated bytes were retained/rejected. The
successful light lock reuses exact original libc0.2.189 and sha20.11.0 records.
Local Python lacked tarfile's filter argument; corrected extraction checked every
flat regular member and used exclusive file creation. `harness-failures.json`
retains these wrapper outcomes. None is relabelled a product defect or pass.

Coordinator work owed: review source/Refusal migration, actual full public caller
and downstream consumers; current-source meaningful deadline/identity/allocation
guard removals with forbidden-effect witnesses and restored checks; process exit
before/during handshake and borrowed-drop lifetime; repeated native Windows
handles and zero/overflow/interruption/backlog/partial-I/O cases; complete API and
simulator corpora; native Macarm64/Windows/Linux GNU+musl and seven-target lint;
actual supported VM hosts and resource cleanup; all audits/profile/benchmark/
chaos/sweeps and comparative targets. Paused Windows ARM remains untouched.
MR-OOM-MAC-1 remains separately open. This checkpoint is for coordinator review,
not completion of G22 or any managed-runtime phase.

## R01–R16 accounting and coordinator handoff

| Rule | Scoped evidence and remaining obligation |
| --- | --- |
| R01 | New borrowed errors/array bounds/checked deadline and RAII paths are typed; 7 focused cases plus the separate ready-I/O case pass, strict lib clippy passes. Allocation injection is root-owned/unrun. Whole-library production panic freedom and MR-OOM-MAC-1 remain open; root audits required. |
| R02 | Exact reviewed candidate, current main seam and primary alternatives above; no new architecture selected. Root reviews the adaptation and external consumer compatibility. |
| R03 | Protocol/u32 array bounds, OS access flags/layout and timeout units; no timeout default or arbitrary cap. Test-only inputs named above. |
| R04 | Explicit acquire/check/exchange/recheck/return transitions and typed finite/identity/I/O/refusal outcomes; root lifecycle failure/cleanup campaigns owed. |
| R05 | Existing shared handshake/Socket/process-hold seams, readiness notifications plus caller deadline; no new cache, watcher or periodic polling. Owned existing pacing unchanged and not newly validated. |
| R06 | One finite core, owned compatibility adapter and concrete borrowed function; focused code review by implementer, root audit owed. |
| R07 | Comments explain protocol/deadline/ownership/unsafe/refcount obligations; no narration requirement. |
| R08 | No comparative result; equivalent 4–5× target, distributions and misses remain root work. |
| R09 | Historical actual red retained with full source; current adaptation is feature work, not a newly reproduced claim. No remedy for the separate owned Mac abort claimed. |
| R10 | Focused reproducible harness, exact source/results/hashes, historical raw provenance and owed coordinator work retained here; current meaningful guard-removal/effect evidence owed to root. |
| R11 | Only actual named light host results count. All seven targets/native Windows/macOS/Linux-libc and supported VM combinations owed; Intel Mac unsupported and paused WinARM untouched. |
| R12 | Existing shared client/Socket/afunix/platform seams and held identity; no kernel/VMM, root/admin, installed binary, security or crypto change. |
| R13 | Required concrete caller and minimal shared implementation; no manager, generic framework or speculative API. |
| R14 | No allocation/fault/CPU/latency/throughput/resource-performance claim; all profiling/comparison is root-owned. |
| R15 | Native implementer; no children/main merge/audit/heavy campaign. Signed named-path checkpoint and exact owed combined/source runs returned to root. |
| R16 | Existing worktree reused; Mac free-space floor checked before work, no local build/new target/full copy/cache deletion; remote light only private new paths within1CPU/1GiB/2GiB admission. Evidence/source/other implementers preserved. |
