# Migration review fixes, 2026-11-13

POLICY_READ: Shards base `9efde72524dd626f3ff84e6b9891852d7c810a9b`; role implementer,
area migrate04–10. Rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full policy-file hashes are in [the review](../../../bugs/2026-11-13/migrate.md).
Current fix-brief/common-brief authorize focused remote checks and signed ff landings.
ROOT owns VM tests, resource-fault campaigns, measurements and cross-platform gates.
Migrate03 belongs to the memory reviewer; P0s migrate01/02 belong to Claude.

## migrate06: deadline shaping respects the baseline

The host capability had been used after the baseline removed CPUID1.ECX24, restoring a
feature that the fleet CPU did not admit. KVM now intersects the capability with that
baseline bit before shaping. With no baseline native behavior remains; a nonempty
baseline missing leaf1 admits none of that feature. No tunable or new refusal is added.
The CPUID register/bit is Intel SDM Vol.2A CPUID's TSC-deadline feature, an architectural
bit, not a learned value. This preserves the existing CPU-coverage refusal semantics.

Remote runner: `adacachy2`, Linux x86_64, cold target
`~/g121-tmp/fix-migrate-v1/target`, `CARGO_INCREMENTAL=0`, commands at nice10, one cargo
at a time. Source transferred by a small bundle carrier; no local builds/tests.

- r1 / `54de9a4ae`: fixture compile failure (private helper), **not** a defect reproduction.
- r2 / `9add07035`: `cargo test -p shards-vmm a_cpu_baseline_keeps_deadline_timer_shaping_within_its_features`.
  One test failed on the claimed symptom: exposed bit was 16777216, expected zero.
- r3 / fix `a09a09f0c`: `cargo test -p shards-vmm arch::x86_64::cpuid`:
  10 passed, 0 failed; integration binaries had no matching tests.
  `cargo clippy -p shards-vmm --all-targets -- -D warnings`: passed.
- Main merge and post-merge focused result are recorded in the landing receipt below.

Owed to ROOT on a suitable two-host KVM pair, with source deadline capability and
destination without it: configure their common baseline and run
`SHARDS_MOVE_INVITE=<destination-invitation> cargo test -p shards --test moved a_microvm_moved_to_another_host_keeps_its_cpu_image_files_and_connections`.
Also run `cargo test -p shards --test moved an_x86_cpu_baseline_is_all_its_guest_finds_and_goes_with_it`.
These are real-VM checks; neither ran here. No speed, allocation or fault claim is made.

## R01–R16 evidence scope

R01: pure bounded fixed-array lookup, no new fallible allocation or panic path.
R02/R03: existing KVM capability and architectural feature-bit contract; no new tuning.
R04: native/baseline admission explicit, existing typed CPU coverage preserved.
R05/R06/R07/R12/R13: one shared CPUID decision at the existing backend seam; no new wait,
cache or manager. R08/R14: no optimization or comparative claim; measurements owed.
R09/R10: exact unit red/green above, reproducible test retained.
R11: Linux x86_64 focused checks only; CI matrix and real hypervisor runs owed to ROOT.
R15: implementer-only light scope, signed commits, main merged before ff push.
R16: only the task-owned remote target is used and must be removed after the last slice;
no other job/cache is touched.

## Remaining admission and hardware work

### migrate04 — owed to ROOT: independent state admission

No replacement cap was added. `most_state` trusts an unadmitted RAM claim, and there is
no independent read/restore budget in the existing `snapshot::read`, `backing_named`,
`relocate` or move-import API. Replacing saturation alone would still admit any finite
host-exhausting claim; a fixed MiB cap or a fraction of free RAM would invent authority.
Mmap alone would trade the allocation for a truncation/SIGBUS hazard.

Design owed: the destination's trusted restore/import owner reserves a budget for
state and guest memory before whole-file reads. It supplies an explicit admitted limit
through every read path; checked header arithmetic and nested lengths must fit it.
Reservation is atomic across concurrent restores and released on refusal/cancel.
Cold start uses local configured authority plus observed resources, never the source's
claim. Unknown/unavailable resource observations produce a typed refusal; no fallback
silently enlarges permission. Prefer incremental decoding so malformed trailing sparse
bytes do not force committed allocation before structural validation.

ROOT's resource-fault fixture must cover `relocate` through migration import, not only
`decode`: sparse multi-GiB state, valid architecture/header, forged huge `memory_mib`,
under a disposable process memory limit, with parent RSS/commit and cleanup observed.
The proposed regression seam is `snapshot::read` with an injected admitted budget;
`cargo test -p shards-vmm a_sparse_state_cannot_exceed_restore_admission` is the intended
command **after ROOT implements that fixture**. It does not exist or count as run today.
No live/OOM reproduction or fix is claimed.

### migrate07 — owed to ROOT: XSAVE layout and host-pair validation

The source guard is real, but a blind zero-padding change is not sufficient evidence of
compatibility. [Linux v6.18 KVM API, section4.43](https://github.com/torvalds/linux/blob/v6.18/Documentation/virt/kvm/api.rst)
says SET_XSAVE consumes the destination VM's CAP_XSAVE2 bytes and state offsets follow
**host** CPUID0xD. Therefore the proposed fix must validate the saved guest components
against source/destination component layouts, copy by admitted component offsets into a
fallibly allocated destination-sized buffer, zero absent state and preserve the existing
CPU-coverage refusal. Unknown/incompatible layouts must refuse typed. Padding the source
blob without checking layout would hide an unsupported case.

ROOT must first retain non-AMX/AMX same-vendor host data (CPUID0xD plus VM CAP_XSAVE2),
then drive the already documented two-host move command above with their common
non-AMX baseline, verifying guest vector state and progress. Add a pure buffer/layout
regression after the minimal actual host pair is retained. No KVM VM was created here
and no XSAVE implementation was changed.

### migrate09 — owed to ROOT: measured hold period

D191 already specifies `P = vcpus * g / share`, where g is the larger of measured wake
resolution and thread CPU-clock resolution. Cold start uses measured soonest wake until
both observations exist; failure to observe is typed, never a baked-in per-host number.
ROOT owns the kick-cost and contention measurements prescribed by D191 before this
policy is implemented. Neither100ms nor another chosen period was installed here.

### migrate10 — owed to ROOT: learned stopping and admitted speculation

Keep byte/page wire units and architectural bounds distinct from policy. Derive whether
another round pays from observed dirtying, transfer service and destination readiness;
a cold link has unknown costs, so begin with a bounded admitted transfer and use actual
progress before predicting downtime. Stop when marginal progress no longer pays, with
cancellation/resource authority bounding work rather than a chosen round count/factor.
A destination supplies its admitted private prefetch budget; reserve it across the pool
and release it on cancellation. Learn record/queue sizes from the resource allowance
and actual service/consumer waits instead of fixed1MiB/two-frame choices. Snapshot
publication retries follow observed generation replacement/progress and cancellation,
not a picked count. Recording ends at an explicit startup observation, with measured
resource cost/refusal for a start that remains unobserved. ROOT still owes cold-link,
contended-host and large-working-set measurements plus cancellation/failure fixtures;
these are designs to validate, not implemented algorithms or passing performance claims.

## migrate08: Linux feature names reach the baseline map

The OS-name/register map now lives in the shared Arm CPU seam; daemon readiness imports
it on Arm. Linux v6.18 cpuinfo HWCAP names (`aes`, `sha2`, `atomics`, etc.) now map to the
same architectural registers as the macOS spellings. Register encodings are architectural
identities, not tuning. No new error/refusal or authority enlargement is introduced.
Unknown and multi-register names remain checked by name: a partial baseline cannot
silently drop their compatibility check. This conservative residual can still refuse
some otherwise compatible moves and is not claimed as complete Linux feature coverage.

Primary sources: Linux v6.18
[CPU names](https://github.com/torvalds/linux/blob/v6.18/arch/arm64/kernel/cpuinfo.c),
[HWCAP field definitions](https://github.com/torvalds/linux/blob/v6.18/arch/arm64/kernel/cpufeature.c),
and Arm DDI0487 ID-register encodings already used by this module.

- Red at `ae37abb52`, r7: `cargo test -p shards-vmm linux_arm64_feature_names_follow_their_baseline_registers`;
  one failure, `aes` returned None instead of `Some(49200)` (0xc030).
- Green at `f004611b5`, r8, adacachy2 Linux x86_64:
  `cargo test -p shards-vmm arch::aarch64::cpu`: 3 passed;
  `cargo test -p shards-vmm arch::x86_64::cpuid`: 10 passed;
  `cargo clippy -p shards-vmm -p shards --all-targets -- -D warnings`: passed.
- Clippy used `SHARDS_HELPERS=skip` and prebuilt placeholder guest paths `/bin/true`
  (`SHARDS_INIT_BINARY`, `SHARDS_SERVER_BINARY`) to avoid nested guest/helper builds.
  No VM test ran; placeholders are never runtime-validation evidence.
- ROOT owes the above two-host move command on Linux arm64 with a baseline masking a
  source-only mapped HWCAP; native backend and Windows/macOS checks remain unrun.

Post-merge receipts: r4 and r6 reran migrate06's10 CPUID tests plus clippy successfully
on their then-merged trees; policy r5 passed. Earlier ff pushes were rejected as main
advanced, so no landing is inferred from those checks. The final landing SHA and its
new post-merge result must be retained in ROOT's receipt.

## Resumed landing receipt

ROOT's corrected handoff supersedes its earlier P0 handoff. The unlanded P0 tip
`21c66fd3e` was briefly merged locally, never pushed, then the merge was removed
with `git reset --keep 09338f18d` at ROOT's explicit instruction. That reset preserved
the owned untracked hardlink regression fixture. The CPU landing must not contain
`21c66fd3e` unless its owner independently lands it on the verified remote main.
Remote authority is `git ls-remote origin refs/heads/main`, not the shared tracking ref.

- Focused merged tests at `8eb8767d6` (original run r10): 3 Arm CPU tests and 10 x86
  CPUID tests passed; policy check passed. Both crates' clippy passed at `f746ce4c3`
  (original r9), and at the earlier green fix `f004611b5` (r8).
- Resumed check r2, `09338f18d`: `cargo check -p shards-vmm -p shards --tests`
  passed on adacachy2 Linux x86_64, warm target, no VM run. This tree integrates the
  memory lane's `29196c6b3` owned-copy fix; migrate03 is not implemented twice.
- Subsequent main merges follow ROOT's amended fix brief: check overlapping crate
  inputs on the warm target; rerun focused tests after any conflict; disjoint merges
  do not pretend to be separately tested.

Migrate05 remains held for the P0 extractor owner's actual remote-main landing.
The archive regression already failed at `8eb8767d6` plus the retained patch (original
r11): after importing hardlinked a/b, writing `after` through a left b reading `before`.
Command: `cargo test -p shards a_volumes_hardlinks_keep_shared_contents_after_move --bin shards`.
This is a real file/archive unit failure, not a VM test. The test will be committed into
that slice once the P0 extractor seam lands; no production extractor change is included
in this CPU landing. ROOT accepted migrate04/07/09/10 as owed with the designs above.

## CPU landing and held hardlink slice

`git ls-remote origin refs/heads/main` confirmed CPU06/08 landed at
`24402e230fd1930fed6afdca2c870660ba16c51e`. This tip excludes the unlanded P0
`21c66fd3e`; no P0 code was published by this lane. It merges remote main
`b6c76d45c` after check `dcbfa8b19`.

An exact-SHA readback found the carrier runner still at the prior checked tree,
so a single post-landing check explicitly fetched/checks-out `24402e230`, asserted
HEAD equality and ran `cargo check -p shards-vmm -p shards --tests`. It passed on
adacachy2 Linux x86_64 (resumed Kiln r2). The complete command output remains in
`~/g121-tmp/fix-migrate-v1/landed-24402-check.log` on that runner. Earlier r2 IDs in
this document belong to the original turn and are distinguished by source SHA.
Only the lane's target directory was removed afterward; source and evidence remain.

The migrate05 regression from original r11 is now a committed source test, not an
untracked patch. It remains intentionally red: importer alias loss is unfixed.
No production Placer/Writer change is included. ROOT's corrected instruction holds
that implementation until the P0 owner's extractor fix is actually on remote main.
This local held slice must not land by itself; its test becomes green with05's fix.
