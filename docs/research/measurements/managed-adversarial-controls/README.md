# Authenticated adversarial application controls

This is an implementation checkpoint, not a passing live campaign or managed-plan
closure. Base `cbd39284937d024640c56db6f8d8ef2bd9b0ee24`, assigned adversarial
implementer Kiln `01a1c5d2ee5871a2a9a9cdc9f56793f0`, coordinator
`01a148884a4770fdaf0111f3f792a248`. Current rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Current R15 authorizes this lane's bounded live campaigns; the older handoff's
light-only limitation is superseded. ROOT retains audits, profiling,
benchmarking, integration, evidence review and main merges.

Owned source is only the new `adversarial_controls.rs` leaf and its children.
`register.patch` and `raw-control.patch` are **unapplied ROOT-owned parent edits**.
No production, ABI, API, other implementer's fixture or protected cache changes.
Earlier refactor/operator/malformed branches and c705/79f native fixtures remain
preserved. The latter already cover initial cursor/scope/generation refusals;
these cases require an actual record before adversarial application control.

## A: record-derived invalid ACK

One selected `ack-partial`, `ack-replay` or `ack-forward` and channel starts a
fresh disposable owner-output run and a fresh authenticated target endpoint.
The real canonical Open is decoded with the checked ABI and accepted by
`StreamState`. Replay first ACKs that Open and actually observes subsequent Data
before replaying the previous ACK. This causal step also avoids a second command
being mistaken for the separate bounded-command Full behavior.

Partial and forward cursors derive from the observed record's start/end. They
are application wire cursors, distinct from canonical payload offsets. The next
actual event must carry typed `Reason::Protocol`; unexpected records are saved
as original bytes before the assertion fails. Local driver stop/join happens on
both outcomes. A successful case requires no further queued target record and,
after owned run cleanup, exactly one aborted/unknown metadata row with accepted
equal to the held record end and consumed equal to the last valid ACK. These
witnesses test false credit/future output; command enqueue alone proves neither.

The other member1 channel must finish canonical End, its ACK and actual transport
EOF. Member2's initial fd1/fd2 bytes are consumed independently. Only after the
target refusal does the existing published TCP gate release member2, which must
emit exact fresh `harness-after-exit` bytes on both channels. Independent main
exec progresses before and after that release. No replacement target connection
is attempted: the separately open reconnect-admission failure is not bypassed.

The owner stop, inspect `false 0`, remove, metadata unlink, observer join and FD
baseline are recorded before final metadata assertions. Any protocol panic is
retained and cleanup is attempted before it is rethrown. Full guest population,
wait statuses, parent/kernel charges and remote connection drain remain Unknown.
The reused release helper's historical JSON label mentions both member1 EOFs;
this fixture's `before-release.json` is the precise witness (one target refusal
and the other channel's canonical End/ACK/EOF), not two normal target EOFs.

## Bounds and invocation

The ignored test is
`tenant_exit::adversarial_controls::invalid_application_ack_after_record_preserves_credit_neighbor_and_main`.
Each process receives explicit `SHARDS_ADVERSARIAL_` variables: `CASE`,
`CHANNEL` (`stdout`/`stderr`), nonzero guest `PORT`, positive `PHASE_MS`, `WORK_MS`,
`CLEANUP_MS`, `OUTER_MS`, full `SOURCE_REVISION`, and new absolute `RECORDS`.
Phase and cleanup deadlines are anchored to the test's first `Instant::now`;
checked phase+cleanup must fit the supplied outer bound. No deadline reset,
retry, sleep or periodic probe is added. The admitted direct caller must also
enforce OUTER_MS on the test process; source-side deadlines cannot bound a stuck
kernel call or artifact resolver. No new launch wrapper is provided.

Use the actual full-source caller and matching host/guest artifacts. Records
contain fixture/CLI/testguest/kernel/init hashes, compiled leaf/contract hashes,
caller-declared revision, raw control/record bytes, actual typed cause, independent
progress and cleanup outcomes. A caller must retain its exact build command,
source/build identity and raw stdout/stderr. A source label is not a binary hash.
Run one selected case/channel per fresh process/records/run. Never silently reuse
a record directory, retry a failed case or call a filtered/ignored test a pass.

## Actual checks at this checkpoint

Fedora `8fadc1ec213332e16cb0d915a3fb913f`, Linux6.19.10-300.fc44.x86_64,
glibc2.43, admitted **1 CPU / 1 GiB / 2 GiB**. New private
`/home/adalundhe/managed-adversarial-controls-v1/{source,target,records}`;
no ROOT target/cache reuse. Preflight observed accessible `/dev/kvm`; no VM was
started by that check. Initial shell Rust1.99.1 was rejected as the build pin;
all compilation below explicitly used Rust **1.98.1**.

Actual Kiln **r37 exit0** compiled the unchanged canonical ABI/build.rs and exact
owned `contract.rs` directly with pinned rustc, then executed **3 pure tests,
3 passed / 0 failed / 0 ignored**. The target occupied 13,549,046 bytes. These
check record-derived cursors/overflow, real ABI wire-versus-payload offsets and
anchored finite bounds. They contain no simulated Client, Driver, endpoint or
lifecycle model. Exact commands, raw stdout/stderr/exits, source hashes and
binary identities are in `light-r37/`. ABI source matches main; no ABI test edit
or identity override was introduced.

Local rustfmt used canonical `rustfmt.toml`, edition2024 and `skip_children=true`;
check passed. Both unapplied patches pass `git apply --check` against current
main. `scripts/check-development-policy` passed. Unrestricted staged
`git diff --check` reports the actual rustc-test stdout's final blank line and
unified patches' blank context prefixes. Those original bytes are preserved;
the source/prose check excludes only `light-r37/5-stdout`, `register.patch` and
`raw-control.patch`. No repository whitespace rule/configuration was changed.
No local compilation occurred;
no active local target/cache. Full leaf typecheck/Clippy and live case results
are still unrun. The full caller needs ROOT's registration and an admitted build
envelope; no old passing binary is presented as this new leaf's gate.

## B/C continuation and decision

[Raw-control proposal](RAW-CONTROL-PROPOSAL.md) records actual callers, primary
contracts, alternatives and exact unapplied receipt patch sent for ROOT review.
Existing pending/partial-write/readiness handling must remain authoritative.
ROOT approved both exact patches for this lane's private candidate/campaign and
eventual reviewed composition. Source parents remain unchanged in this worktree;
the private campaign applies the approved patches. Raw patch SHA-256
`0178601648d07a27eff18f944eb03ac775c121190bc059cb88fb86bbda8923ee`, registration
SHA-256 `41e04e2d0b1099aae02d698bb1f8cb7c635c99e90bda76549412014a4e090489`.

B now implements `control-unknown-tag`, `control-wrong-tag`,
`control-overdeclared`, `control-ack-length`, `control-short-header` and
`control-short-ack`. The real consumed encoder supplies the base; checked frame
peek/put establishes the selected framing. Actual bytes never exceed the
existing CONTROL_BYTES and the declared huge length is only a header. A real
observed Open precedes the one raw command on a fresh endpoint. Malformed cases
require Protocol; incomplete header/body plus FIN requires Closed under the
current Service contract. Every actual receipt is saved before checking complete
positive writes/remaining-pending consistency and the exact requested FIN result.
Read counts and send acceptance are not application consumption or durability.

C implements `terminal-end-duplicate`, `terminal-end-forward` and
`terminal-end-finish`. Actual canonical End, its single ACK and transport EOF
precede the invalid control; final metadata must remain ended/complete with
unchanged accepted/consumed. The same surviving-neighbor/main and cleanup checks
apply. No canonical Aborted(errno) trigger has been established in this fixture;
it remains owed and is not inferred from an actor exit, refusal or owner stop.

Actual ARM64 Mac d132 private `~/managed-adversarial-controls-light-v2` r41 exited0
under 1CPU/1GiB/2GiB: 4 pure contract +2 pure write-receipt tests passed, both
scoped Clippy invocations with warnings denied passed. Raw commands/exits and
hashes are retained in `light-r41/`; target4,404,488B, disk remained >30GiB.
Those tests predate adding Control selection to the contract; they are not the
current full caller's gate. Only unchanged `write_receipts.rs` and ABI bytes match
that snapshot now. The whole leaf/control_inputs were saved but not compiled in
that light run. Maccec preflight observed26,725,994,496B, refused before source
creation/build; Mac2768 remained unused due its reported disk floor failure.

Next is the full normal caller/helper build and bounded selected live cases in
Fedora private `~/managed-adversarial-controls-live-v1/{source,target,cargo-home,records}`,
2CPU/4GiB/16GiB subject to runtime admission, build1800s/outer2100s. Case inputs:
phase300000ms, guestwork240000ms, cleanup120000ms, outer480000ms, explicit guest
port and one selected case/channel per new process/records/run. No protected
ROOT target/clone/frozen helper is used. No current full caller compilation or
live pass is claimed at this source checkpoint. Current
End versus Aborted(errno), logical terminal versus transport and owner lifecycle
contracts remain strict; no speculative production correction is authorized.

## R01–R16 account

| Rule | Evidence / limit |
| --- | --- |
| R01 | Test-only assertions/panics; production unchanged. Full caller lint unrun. |
| R02 | Exact Client/Observer/Capture/Service/Io callers inspected, native c705/79f frozen source read. RFC9000 and rejected transport-copy/raw-UDP alternatives in proposal. No architectural choice. |
| R03 | No production literals. Explicit caller budgets/port; invalid cursors derive from observed interval and checked ABI lengths. |
| R04 | Existing typed StreamState and Reason checked; explicit selected ACK case, held cursor and last valid ACK. Enqueue, observation, metadata and ownership have different witnesses. |
| R05 | Existing authenticated Driver and normal fixture reused; event recv/deadline, no new polling/cache/manager/thread. Receipt proposal has no authority. |
| R06 | Focused input, trace, observed-record, run and cleanup functions. No new generic framework. Full compile still owed. |
| R07 | Comments explain local-join versus remote-drain and evidence obligations. Historical helper label limitation disclosed above. |
| R08 | No performance/comparative claim; ROOT owns benchmarks and 4–5× target remains unverified. |
| R09 | No production fix or new confirmed bug. Source expectations remain unverified until actual live run. |
| R10 | Raw light commands/exits/source/binary hashes retained; live fixture retains original bytes, typed events, progress and cleanup before final assertion. Live/mutation records pending. |
| R11 | Actual light check is Linux amd64 GNU only. No live or seven-target claim; macOS ARM64 only, paused WinARM untouched. |
| R12 | Existing shared ABI/remote driver and normal fixture, no OS adapter or duplicate transport. |
| R13 | One selected fresh target, bounded finite work and cleanup. Parent changes remain smallest unapplied seams for ROOT review. |
| R14 | No profiling or allocation/leak/latency claim. Logical credit and FD witnesses do not prove population/kernel accounting. |
| R15 | Current assigned adversarial authorization acknowledged. No children/goals/wrappers, no main merge. ROOT audits/profiles/benchmarks/integration; this lane continues bounded A/B/C. |
| R16 | New admitted private small target only; no cache/source deletion or heavy local build. Local30GiB floor remains binding. |

Owed: approved parent registration, exact complete caller build/Clippy, actual A
selected cases/channels and forbidden-effect/cleanup records, B seam review and
real malformed cases, C terminal variants, meaningful corresponding mutations
and restored results. ROOT owns combined seven-target/integration and all
audits/profiling/benchmarking. No full managed-plan or durability claim.
