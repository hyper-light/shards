# Owned sandbox workers: native helper-loss correction

POLICY_READ: implementer; full canonical AGENTS/R01–R16/evidence/CLAUDE, managed
runtime plan and current handoff read at main
`cbd39284937d024640c56db6f8d8ef2bd9b0ee24`; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
ROOT assigned the bounded owned-helper native correctness cases; ROOT retains
audits, profiling/benchmarks, integration acceptance and main merges.

This checkpoint changes only sandbox.rs helper_calls and Worker/serve_helper.
The exact candidate file is SHA-256
`3d4acf45f97e023f0d789f4dd8492f2c140ffd6c11e81f8045e32ff45b19ba2e`;
the baseline is `365eed67b1433b694caa40132fbd9958b40a30193dabbfcb3a270491d6e235a3`.
[source-identity.json](source-identity.json) verifies untouched frame/worker/
transport functions and all original tests. No Programs, dispatcher, ABI, Cargo,
init, kernel, VMM or public execution authority changed. Earlier API/refusal
branches and artifacts remain preserved.

## Actual failure and chosen mechanism

The native musl helper ran a real QuickJS worker until its actual receive CALL.
The fixture independently held that worker's pidfd, ended its helper, then
observed **actual DONE completion after helper death**. Both targeted cases failed
for that ownership violation, not merely for missing exit notification. The
helper SIGKILL case exited -9; the invalid-control case exited -31/SIGSYS, whose
exact helper-fatal cause remains unknown. Each worker subsequently exited status0
and was actually reaped by the disposable fixture subreaper; FD counts were5→5.
See the original received baseline output in [receipts.json](receipts.json).

[RESEARCH.md](RESEARCH.md) records pinned Linux6.18.48 kernel ordering, parent-death
race semantics, runc/youki alternatives and the native method before code changed.
Both guest musl [libc typechecks](libc-uapi-typecheck.json) pass; the official
published libc archive matched the repository checksum and contains both target
types ([source search](libc-uapi-source.json)). No duplicate CloneArgs or new
shared ABI module/dependency is needed.

The helper now reserves Worker and next pollfd capacity before creation, uses
clone3(CLONE_PIDFD) without an ownership-losing fallback, and arms SIGKILL plus
checks its pre-birth parent in the child before actor instructions, allocation
or the worker filter. The existing worker filter prevents undoing that link.
One extra temporary descriptor slot is required while the parent's program
socket and atomic pidfd coexist; insufficient descriptor capacity refuses birth.
This is required ownership, not an arbitrary capacity increase.

Worker phases are Running/Stopping/SignalUnknown. On control EOF/error, every
owned worker is signalled before waiting for any worker. A failed signal uses a
nonblocking reap attempt; reap/signal uncertainty remains typed and counted with
the first affected ID/cause. No repeated process probes or new retry loop is used.
Normal reaping removes tracking only after successful waitid. ENDED packets are
stack-built from their existing wire widths; creation refusal preserves the
existing ENDED(id,-1,-1) generic refusal contract. This does not add detailed new
resource causes to that old wire. IO diagnostics keep kind/rawcode without an
allocating OS error-string fallback. Existing initial filter String handling and
other old worker/control allocations are not claimed allocation-fault safe.

## Commands, source and retained results

Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, Linux6.19.10 x86_64, pinned
Rust1.98.1, Zig0.16.0, private
`~/managed-helper-ownership-cbd-before-v1`. Each attempt used a fresh private
source/target/Cargo/Zig cache, admitted1CPU/1GiB/2GiB, no protected root paths.
The actual driver invocation is
`python3 run.py SCOPE 600 40 3 x86_64-unknown-linux-musl`:600s build/check,
40s case,3s notification budgets, one already-allowed CPU inherited by children.
No host limit, service, trust, security setting or paused VM was changed.

[prepare.py](prepare.py), [helper-main.rs](helper-main.rs), [run.py](run.py) and
[observe.py](observe.py) reproduce the small full-sandbox/real-QuickJS harness.
The isolated quote function is copied exactly from the actual baseline server;
no transport or worker is mocked. The small library wrapper is for scoped lint.
Its release fixture profile is not the full carried guest binary's acceptance
gate. [light-Cargo.lock](light-Cargo.lock) is byte-identical to the actual
feature-reduced lock, SHA256
`b85862e582dae5b92ca1ad2daf5fe9be2769c5fc1985573acd56570d470c8f1f`:
11 existing dependency records plus the fixture, unchanged versions/checksums.

| Actual job | Source / result |
| --- | --- |
| r336 before-musl-v4 |313 verified source hashes; build0, death1, control-error1; actual surviving worker DONE in both cases, no timeout. Binary084275916228cf4038a91dd9eace1ce61bc57f5724612753ffe8d257103ab8b2. |
| r342 after-musl-v1 |315 verified source hashes; build0, death0, control-error0, strict library Clippy0 with original denied panic lints and `-D warnings`; no timeout. Binary13438cc87ef8d5e5b2ab4f1b383e6464a818d1ec822432e25425fcfe28e237b5. |
| r345 mutation-death-v1 |Only PR_SET_PDEATHSIG's SIGKILL argument changed to0; getppid guard retained. Build0, death1, control-error0, strict library Clippy0; no timeout. Mutation sourceba8bb511e0edf47ca9568b6c06f03fb8eb810f8987105f9334e4615173104c35; binary2e7c4b41d24448bae0efbffdb216a6acd0c9f75e6f5b27e7d22831c0d830ff3d. Detailed failure reason/DONE receipt not yet retrieved: not claimed a completed effect mutation proof. |

The driver exits0 after retaining per-case statuses; **driver0 does not turn
before/mutation case1 into a pass**. Original raw command logs, manifests,
libraries and binaries remain preserved in their named remote scopes. Received
baseline and mutation job output and transcribed after counts/hashes are in
receipts.json. Detailed after/mutation raw transfer and fresh-process restore
checks remain owed because new runner reads were refused at zero idle cores;
ROOT also reported a connectivity loss. No result is inferred from silence.
The canonical source always retained its armed guard; mutation was confined to
a separate private source. This signed checkpoint is the tested production piece,
not final acceptance of outstanding evidence gates.

The after fixture requires actual SIGKILL wait status for helper-death cleanup.
When an error lets the helper reap its own child, ECHILD alone is rejected: the
independent held pidfd must have HUP and fdinfo Pid:-1, and the worker socket must
retire with no subsequent bytes. Linux6.18.48
[pidfs.c](https://github.com/gregkh/linux/blob/v6.18.48/fs/pidfs.c) distinguishes
exit from reaped HUP and exposes that fdinfo identity. A nonparent exit status is
explicitly unknown. Full kernel charge return is not inferred from these facts.

Initial failures are preserved: admission refusals launched nothing; first
packaging had an ambiguous getrandom version; first native lock was rejected by
`--locked`; generate-lockfile selected newer cc and was rejected before build;
offline metadata then retained exact existing versions while dropping unrelated
cc-parallel edges. Native GNU r329 built0 but reset before CALL, with actual
ENDED code3/status31(SIGSYS); the denied syscall is unknown and no
GNU filter workaround was made. Those are not ownership baseline reds or passes.
All remote attempts, archives and original records remain preserved. Current
packager reconstruction matched the actual lock hash; its315-file source archive
is2162096B, SHA256 eb06af8f8c6417a69ba4183413a25e2a506e8204d3f54e55bf2e7db9e42a02df.

## R01–R16 and remaining gates

| Rule | Evidence and limit |
| --- | --- |
| R01 | Fallible tracking/poll admission, stack reply and nonallocating typed IO cause; strict library Clippy0. Old worker/control/setup allocation faults and helper stdio fatal cause remain unverified. |
| R02 | Pinned primary sources/alternatives, actual ready-worker survival before code, both libc types, source identity and actual after cases. |
| R03 | No picked production tuning value: OS flags/UAPI fields and existing wire widths; all fixture budgets explicit inputs. |
| R04 | Owned pidfd, explicit Worker phases, refusal/stop/reap/error and cleanup-unknown data; old generic creation-refusal wire preserved, no invented generation semantics. |
| R05 | Existing helper/Worker/readiness/control seam; no second manager, cache or periodic probe; derived poll capacity admitted before birth. |
| R06 | One shared helper loop and finite signal/reap traversal; typed local outcomes, source-only scoped change. |
| R07 | Safety/ordering/UAPI comments; source/primary records document what syscall ordering alone cannot prove. |
| R08 | No performance result or4–5x claim; ROOT comparisons owed. |
| R09 | Actual musl after-helper-death DONE in both before cases; GNU SIGSYS separately retained, no guessed fix. |
| R10 | Reproduction scripts, pinned inputs, received receipts and retained private raw files; mutation reason/restore/raw archive remain explicitly owed. |
| R11 | Actual native x86_64 musl, two guest-musl UAPI typechecks; full supported seven-target/VM/current caller gates ROOT-owned. Intel Mac unsupported; paused ARM VM untouched. |
| R12 | Existing single Linux guest mechanism/libc UAPI for both architectures/all host backends; no ABI/dependency/installed binary/authority change. |
| R13 | Ownership fixed at existing helper seam, preserved worker/transport/frame bodies, no parallel execution mechanism. |
| R14 | No profiling/benchmark, allocation-free or leak-free claim. Private operational storage observation before after-build was203256KiB; not kernel/resource-cost proof. |
| R15 | Explicit bounded private native assignment; no child agents/goals/main merge; small signed checkpoint returned for ROOT acceptance, no campaign-complete label. |
| R16 | Mac floor checked34.97→34.96GB for tiny typechecks; no heavy local build or deletions. Root caches, source/evidence and paused VM preserved. |

ROOT still owns final combined server caller/VM/platform acceptance, fault/OOM/
descriptor admission and shutdown-race coverage, kernel/parent accounting and
comparative measurements. The full managed runtime plan remains open.
