# Devices review fixes

POLICY_READ: implementer, devices-02 through devices-10; base
`9efde72524dd626f3ff84e6b9891852d7c810a9b`; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
The four policy hashes are recorded in `docs/bugs/2026-11-13/devices.md`.
Task authority: `/tmp/g121-shot/review/fix-brief.md`, then
`/tmp/g121-shot/brief-common.md`. devices-01 belongs to ROOT's Claude lane.

## devices-02

Red revision `f7cbffbb3543c41eaa1a9141cd79d04485561790`, Kiln run r4,
adas-mac-studio-3.local (Darwin arm64). Command:
`CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=$HOME/g121-tmp/fix-devices/target cargo test -p shards-vmm full_span_mapping_translates_an_msi_without_overflow --lib`.
One test failed at iommu.rs:508:24 with `attempt to add with overflow`;
488 filtered out; exit 101. This drives the accepted MAP and MSI translation.
The release zero-progress loop was not executed because it would exhaust memory.

The fix changes only inclusive span arithmetic: while `at < end`,
`end-at-1` is defined; after `last >= at`, `last-at` is defined. Their minimum
is at most `end-at-1`, so adding one is representable and strictly positive.
The mapped physical offset remains bounded by MAP's existing checked physical
end. Source: virtio 1.3 §5.13.6.6. Green revision `2d17282b6765a51ccc70572a30a0ce552ebcc3a0`, Kiln r6 on the same Mac: `cargo test -p shards-vmm devices::virtio::iommu::tests --lib`, 11 passed, 478 filtered out, exit 0; `cargo clippy -p shards-vmm --all-targets -- -D warnings`, exit 0. Both use the same CARGO_INCREMENTAL/target settings as red. Final merged-tree rerun pending. ROOT owes `cargo test -p shards --test hotplug a_hosts_function_is_plugged_into_a_running_microvm` on an assigned KVM VFIO fixture, plus malicious full-span MAP/MSI from the guest. No release OOM loop was run.

Policy accounting: R01 checked arithmetic invariants above; R02/R03 protocol
representation and bounded arithmetic, no new runtime tuning values; R04 existing
MAP status and Option translation preserved; R05 no wait/cache change; R06/R07/R13
local arithmetic change with invariant comment; R08/R14 no performance claim or
profiling (ROOT owns these); R09/R10 focused unit red/green; R11/R12 shared
implementation, one allowed host tested, other matrix and real-VM runs owed;
R15 no subagents, signed commits and focused tests only; R16 own target removed
at completion. No unrelated cleanup, benchmark, chaos or E2E run.
