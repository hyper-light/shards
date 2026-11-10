# Admit finite payload slots before private fixture setup

Signed source `c141f67d01777b93ab0b41f8628a68c9f911a370`, fresh branch
`p/managed-network-admit-first`, exact base
`c2eb4760e8363af0c36adeb01dd08c3d2b9c3b08`. Only
`crates/testguest/src/managed_network/intercept/sock_addr.rs` changed as code
(18 added/8 removed lines). Previous7ee/da7d/4aa checkpoints remain unchanged.
[source-identity.json](source-identity.json) records the new source SHA256
`a7a830c0ad33934ffc5e0d1aba3c7c9d94a3e8685004c01018837ea17be7696d`, matching
the actual copied source compiled by this batch's light check.

Before work, implementer read FULL canonical managed-main-landing AGENTS,
R01–R16/evidence/CLAUDE/managed plan and current handoff. Current origin/main
`42e3f7582d7bb9b5f0579aecb4085de5b1769b5f`, policy files unchanged from
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Coordinator updates override stale handoff jobs: root Fedora r556 and Mac r552
active; root clones/targets are reserved and untouched by this implementer.

## Live basis, exact delta and limits

Coordinator r531 reported exact source7ee full actual host compile0 and four
real profile×mode guests: both fixed-original-peer comparisons0 with two-leg
Binary/Empty/backend-owned-upstream and cleanuptrue; both destination-only
controls101 for actual Mismatch. These are results for7ee, not this reorder.

Coordinator r556 then reproduced this finite below-capacity ordering at7ee:
BUFFER_BYTES17/required18 returns strict101, but Target already reported
namespace4026532020/cgroup_inode21/backend127.0.0.1:36648/responder127.0.0.1:45211
BEFORE typed PayloadRefusal Limit17/18. No aggregate Cleanup/Observed followed.
This establishes unnecessary private setup before refusal; it does NOT prove a
leak or product defect. Raw runtime proof remains coordinator-owned and its
closing mutation campaign was still active when this source was returned.

With coordinator approval, a controller-local immutable concrete Payload value
has exactly two variants: Scripted or Buffered(actual owned Slots). Immediately
after existing PID1 and Deadline checks, Buffered executes the EXISTING
`at(Stage::ReservePayload, Slots::reserve(...).map_err(...))` and `slots.record()`.
Only after both succeed can Namespace/Cgroup/socket acquisition execute.
Scripted creates neither slots nor PayloadSlots record. The same admitted Slots
move once into existing Work::Forwarded at its existing construction site.

There is no new helper/manager/ledger, mutable preparation flag, Option extraction
or fallback. ControllerKind remains immutable selection data for existing
operation/client-role/completion checks. Slot sizes/capacity/overflow/refusal
implementation, record encoding, original deadline, hooks/emitter, decoder,
worker algorithm, client/backend byte semantics, strict original-IP+port predicate
and existing ownership/cleanup behavior are unchanged. A record error now also
precedes these private kernel resources. Later failures drop the held payload
normally; this is not a measured full cleanup/charge-return guarantee.

Existing alternatives and primary contracts are linked in the
[buffered comparison record](../buffered-relay/README.md#sources-and-alternatives).
No new mechanism or policy is selected. Retaining late reservation repeats the
observed unnecessary setup; a general preparation framework or new budget ledger
adds authority/state not needed for this local reorder. The existing payload
capacity envelope still excludes allocator overhead, stacks, console, sockets,
BPF, parent and kernel costs. This is NOT production hard-budget closure.

## Actual light result

r150 admitted **1 CPU / 1 GiB / 2 GiB** by `needs`, selecting Linux amd64 Fedora
`8fadc1ec213332e16cb0d915a3fb913f` with14cores idle/44GB free at admission.
Fresh `~/managed-network-admit-first-proof-v1` source and target; no prior artifact
mtime retained, no root cache reused. Pinned rustc1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, libc0.2.189.

- 30 GNU pure tests passed;30musl pure tests passed.
- Both scoped production panic/unchecked-index/unsafe lint checks passed.
- Only existing pure test functions executed; no fixture sockets/namespaces/cgroups,
BPF load, child client or VM execution. Full host caller/guest build was NOT run.

The source-linked light driver is unchanged. [light-one-cpu.py](light-one-cpu.py)
changes ONLY its four Cargo command scheduling arguments from `-j4` to `-j1`,
records both driver hashes/copies, and confines only this check process and its
descendants to one actual permitted CPU. [light/driver-scheduling.json](light/driver-scheduling.json)
records actual inherited CPUset and selected CPU0; no guessed CPU index, host
configuration, system affinity or other process is changed. Kiln admission is
not claimed as a hard aggregate kernel memory/disk budget.

Exact toolchain/commands/compiler logs/results and two test-binary identities are
in [light/](light/). Original copied sources/binaries/target remain in that remote
private directory; archive `~/managed-network-admit-first-light-v1.tgz` remains
preserved. [light/transfer.json](light/transfer.json):5761B SHA256
`2d7ae990fab90d4a8bff1758c54855c8070d517d3d2c881a7245ccb82efa4222`,21 regular
entries/20 independently verified member hashes, no executable bytes transferred.

Owned rustfmt1.98.1 and diff checks0. Initial local policy check failed because
this sparse checkout omitted `.github/workflows/release.yml`; one attempted
`git sparse-checkout add --no-cone` was unsupported by this Git command. Adding
the tracked workflow directory with the existing no-cone mode resolved setup;
actual `scripts/check-development-policy` then0. Neither was a product/source
failure or a passing gate before correction. No local compiler/build ran.
Mac free before creating this sparse worktree was34867200KiB, above30GiB.
No cache/evidence/source was deleted; no own active job remains.

Replay only the same light scope, a NEW private directory and unchanged admission:

```sh
python3 docs/research/measurements/managed-network-sock-addr/admission-order/light-one-cpu.py \
  "$HOME/managed-network-admit-first-proof-NEW"
```

## Owed coordinator checks

Use the existing [exact host invocation](../buffered-relay/README.md#coordinator-invocation-and-owed-gates--not-run-here)
with source-matching actual CLI/helpers/testguest and caller-selected kernel,
config association, original deadlines and VM/resource envelope. Required host
fixture remains `direct_guest_two_leg_udp4_records_buffers_and_backend_source_change`.
No host-fixture change is required; it retains raw output before assertions.

Root must observe BUFFER_BYTES17 strict101 with exact typed Limit17/18 before any
Target/ForwardTarget/client/hook/worker facts, and independently validate actual
setup/cleanup boundaries. BUFFER_BYTES18 must retain the earlier positive two-leg
behavior and complete owned cleanup. Scripted controls must emit no PayloadSlots
and preserve their behavior. Destination-only tuple mismatch remains a strict
negative control, never compatibility success because a diagnostic driver exits0.
Independent admission/guard-removal/restoration, all actual byte custody/backend
source witnesses, failure/cancellation, full builds/matrix and costs remain root
obligations; none are reported run by this implementer.

## R01–R16 delta accounting

|Rule|Evidence and remaining obligation|
|---|---|
|R01|Existing fallible reservation/record and panic-safe source reused; no new indexing, arithmetic or allocation API. GNU/musl scoped lints0; real allocation/record/OS faults remain root checks.|
|R02|Root-reproduced exact late admission plus prior primary/alternative record; no broader architectural decision. Actual after-fix behavior owed.|
|R03|No literal/cap/timeout added.17/18 are observed caller-input/script-derived boundary facts, not new policy defaults.|
|R04|Approved local Scripted/Buffered owner carries actual Slots once; immutable selector retained, typed refusal unchanged, no fallback. Actual transitions/order owed.|
|R05|Existing ownership/worker/decoder/waits unchanged; no helper framework, cache, polling or parallel manager.|
|R06|Local reorder with one two-mode owner; no generalized phase engine or arbitrary complexity threshold. Coordinator reviews exact small diff.|
|R07|No narration/comment/safety change needed for moved existing operations; existing source rationale retained.|
|R08|No comparative speed/resource ratio or4–5× achievement claim; target remains open.|
|R09|Root r556 actual ordering reproduction authorizes this finite fixture change; no leak/product defect inferred or speculative cleanup repair.|
|R10|Signed small source, exact base/hash, command/log/source/binary identities, retained setup reds and explicit owed live gates. Root raw before/after/mutations remain coordinator evidence.|
|R11|Only Linux amd64 GNU/musl pure/scoped compile/lint; not actual full caller/seven targets/VM hardware. macOS arm64 only, paused Windows ARM untouched.|
|R12|Existing shared guest mechanisms preserved; no new platform/product adapter, deliverable, host root or crypto change.|
|R13|One concrete local owner enables necessary move-only slot transfer; no broad ledger/API or duplicated decoder.|
|R14|No implementer audit/profile/benchmark/resource campaign; actual cost/accounting/retention/CPU/kernel proof remains root-owned.|
|R15|Implementer scoped source+pure/light only; no children/guest/BPF/sockets/ns/mutations/audits/heavy/main merge. Root owns all live checks/review/integration.|
|R16|Unchanged1CPU/1GiB/2GiB admission; NEW private source/target, own check affinity only; Mac floor respected/no local build. Root caches, paused VM, all prior source/evidence preserved; no guard bypass.|
