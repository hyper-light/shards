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
