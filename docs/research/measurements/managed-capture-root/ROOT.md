# Capture storage: coordinator integration gate

Source `09a187d2a55522d59a79b0fc21ac9378f25c0fb9` selects the two reviewed
capture implementation files from the resource lane onto the PID-reset batch.
It transfers the prepared allocation through capture, submission and application
ACK, and charges replacement overlap and actual capacity. The source/evidence
selection is recorded in `7fe4e20c9`; this is not a hard parent/kernel resource
entitlement or a universally enforced RAII loan contract.

Coordinator policy: current main `ca3ab10c151aa5539e757f1cc76029746ccbb561`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Read the linked resource-lane R01–R16 and ownership records before composition.

## Actual Mac gate

On ARM64 Mac runner `b24c5c951260dcdfd0972f2aeaf3cf51`, r935 completed0:

- Full supported seven-target, GPU and formatting lint: exit0.
- Native remote managed units:18 passed,0 failed,0 ignored.
- Actual full-daemon terminal group:13 passed,0 failed,0 ignored.

`mac-fixed.py` is the executed driver. Root independently read all three exit
files, actual source identity and positive test counts from the raw logs.
This is full-daemon compilation/testing, not the implementer's adapted core.
The existing compiler feature-probe warnings remain verbatim in `lint.log`;
they are not silently removed or reported as failing final target results.

[Raw archive and independent member verification](root-records/mac-capture-09a/verified.json):
7018 bytes, SHA-256 `58e2688607a0928aef7f654473f7624225291fd3b1730217bf4f539ace8435cb`,
17 payloads plus manifest,18 unique safe regular members. Root checked every
member's exact length and SHA-256. Original runner files and caches are retained.

The first matrix attempt at `7fe4e20c9` passed its Mac target and failed E0004
on the first Linux target: the native terminal fixture did not match the new
explicit `Capturing` state. The test now rejects that unexpected outstanding
loan explicitly; it does not pretend the loan is ready or terminal.
`first-matrix-failure.json` retains the original failure and stop receipt.

## Original amd64 attempt: no result

Initial r933 was stopped after the known compilation failure; no runtime result
is inferred. Its partial paths remain protected. Corrected r936 used a new
synced source, private target and output directory, but lost contact with
Adacachy. A subsequent lookup found no run. A later metadata read r953 was
refused before execution because no CPU was idle. Actual source synchronization,
build/test completion and termination remain Unknown until the existing files
can be inspected under ordinary admission. No second run has replaced them.

After ordinary runner recovery, a coordinator metadata read found the original
`~/root-managed-capture-09a187d2a555-v1` output directory absent and no matching
driver process. No amd64 result is recovered or inferred. The original synced
checkout/target remains protected. Advancing the old capture branch with an
evidence-only commit before resolving synchronization was a coordination error;
the independent landing starts from current main and selects only named paths.

## Actual amd64 KVM caller on landed main

The fresh no-sync r37 gate checked out immutable published
`0c598386ca1f7c65c6e0634dba77983e4abffff2` into its own source/target on Adacachy.
It did not reuse or overwrite the uncertain original attempt. CachyOS Linux
7.2.9-2-cachyos amd64, actual cached Cargo compiler metadata and post-run readback
identify Rust1.98.1/48a229cea and LLVM22.1.8. Admission was4CPU/16GiB/64GiB,
cargo-j4; neither that envelope nor the observed free space proves a hard cap.

| Actual stage | Result |
| --- | --- |
| Policy | 0 |
| Normal carried managed-output build | 0,296.62s |
| Matching static musl test guest | 0,31.94s |
| Ordinary managed-output KVM suite | **25 passed,0 failed,10 ignored**,43.86s wall |

Each direct subprocess had actual pidfd exit readiness and was waited/reaped;
controller descriptors returned9→9 for every stage. No timeout or SKIP occurred.
Whole descendant/kernel return remains Unknown. The test and guest executables,
source and target remain in the private scope. `amd64-main.py` uses a fresh
short0700 temporary parent for native Unix socket length compatibility; it does
not widen a production limit. Official OTLP consumers were not rerun because
this batch leaves their codec unchanged; no new consumer acceptance is claimed.

[Independently verified amd64 raw archive](root-records/amd64-main-0c/verified.json):
59 payloads plus manifest60,32146bytes, SHA256
`d459c963985eb22df08da96b68a1a6e002d2f7b0abab7747eb9858ec721924af`.
Every member is unique, safe, regular and matches its declared length/SHA256.
The separate post-run toolchain record includes actual cached Cargo metadata,
not a claim that a later command alone identifies the earlier compiler.

## Actual ARM64 KVM caller

The separate existing nested-KVM fixture ran from frozen source09a on this
ARM64 Mac, with normal carried helpers and matching static Linux test inputs.
The outer HVF guest exposed EL2; the inner ordinary `managed_output` suite
actually ran under ARM64 KVM: **25 passed,0 failed,10 ignored**,44.89s. The
outer test passed1/1. This is neither native Mac unit coverage nor an amd64 or
WHP pass. Ignored adversarial cases remain unrun in this gate.

Host carried build returned0/315.01s; the bounded nested launch returned0/405.95s.
The exact driver, console, commands, source/kernel/executable identities and
launchd teardown observation are retained in
[the independently verified archive](root-records/nested-capture-09a-v2/verified.json):
21 payloads plus manifest22,33123bytes, SHA256
`dacc7895fdc93198dea73e288d0c213c4e26408017524ff80600fb09aa619500`.
All original source, artifacts and targets remain. Launchd bootout reported no
such job after completion; whole-population/kernel resource return is Unknown.
The first packer failed before archive creation on Python3.9's missing
`hashlib.file_digest`; its exact source/error remain in the preceding directory.
The corrected packer only reads original data and hashes files incrementally.

## Independent one-guard removal and restoration

`guard-mutation-v2.py` uses a detached source09a worktree, the same canonical
Observer and actual authenticated QUIC/application ACK. Only the undersized
loan check is removed in the private mutant. The case submits one owned byte,
observes its record, acknowledges it and requests the next advertised loan.

| Phase | Actual result after both endpoint actors joined |
| --- | --- |
| Baseline | 1 passed; typed `Admission::Capacity` at advertised58; direct owned58-byte submission succeeds |
| Guard removed | 101; actual advertised58/loan capacity1, exact decisive assertion fails |
| Exact source restored | 1 passed; typed refusal and same owned submission succeed |

All three builds returned0; no timeout or runtime retry occurred. Source restores
in `finally` and hashes back to
`005bdb67ced70ecb26312c5738c9b53eb092c09f7ca84e8f8f75fc9dc536da95`.
This proves the public producer boundary, not that the daemon mixes the two
producer APIs or that every resource failure is handled.

[Guard raw verification](root-records/guard-09a-v2/verified.json):36 payloads plus
manifest37,48896bytes, SHA256
`f82d5813609a2f4bf4b6f00f063a2d821069a7324a3a740defbb0498af3a90ff`.
ROOT verified every unique safe regular member's exact length/SHA256 locally.
The restored executable is retained outside Cargo target. Later Cargo builds
overwrote the baseline/mutant executables: their original hashes, sources and
logs remain, but those binary files are not claimed preserved or regenerated.
`guard-v2-dispatch-and-recovery.json` retains admission and transport failures.
The first dispatch r967 has no recovered output; it is not a runtime result.
Evidence retrieval retries did not rerun a test or change the original archive.

## Landing and remaining gates

The independent landing selects exactly remote/managed.rs, daemon/managed.rs,
the exhaustive native-terminal fixture arm and their dedicated evidence onto
main ca3. `git diff09a -- crates Cargo.toml Cargo.lock .cargo scripts resources`
is empty after selection: the complete compilation/runtime inputs equal the
tested source. Main's newer policy, PID-reset evidence and plan text remain.
`linux-sync.py`, `linux.py`, `mac.py`, `mac-fixed.py` and the nested/guard drivers
are retained evidence, not commands to blindly restart.

R01–R16 review extends the implementer's complete account in
[README](../managed-resource-enforcement/README.md) and
[ownership record](../managed-resource-enforcement/OWNERSHIP.md):

| Rules | Coordinator result and limit |
| --- | --- |
| R01/R04/R06/R07/R13 | Reviewed fallible record preparation, explicit Capturing/growth/revocation/ACK states, checked size arithmetic and caller submit-or-abort ownership. New guard refuses before capture/credit; no generic pool or second lifecycle manager. Full fault-path proof remains open. |
| R02/R03/R05/R12 | Existing protocol/layout bounds and pinned Vec/cgroup sources; allocation follows the existing slot/credit notifications, without polling or a storage cache. No new production default or platform duplicate. Actual storage is distinct from authority. |
| R08/R14 | No comparative speed, allocation-free, leak-free or4–5× claim. Actual allocation/reallocation/copy/fault, induced parent/kernel charges and protected-control measurements remain owed. |
| R09/R10 | Real API red/green, independent exact check removal/restoration, full caller and ARM64 KVM results with verified raw records. Binary-retention and original-attempt limitations above are explicit. |
| R11 | All7 targets/GPU/format compile-lint; Mac native18remote/13daemon; actual ARM64 KVM25 and landed-main amd64 KVM25. Native HVF capture, WHP and all Linux distributions are not established by this batch. Paused Windows ARM stays untouched. |
| R15/R16 | ROOT reviewed source/results/mutation and prepares the signed named-path batch. Original sources, targets, failed outputs and evidence are retained; no user-data/cache deletion or headroom bypass. |

Hard parent/kernel backing, complete transport accounting, actual protected
control, retained payload fault/owner-loss races, performance and the remaining
P0–P13/G01–G25 obligations stay open. This is a partial implementation batch.
